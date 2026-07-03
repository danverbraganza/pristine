//! Stdio transport for JSON-RPC dispatch.

use std::collections::HashSet;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream::{BoxStream, SelectAll};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::harness::SkillsAnnouncer;
use crate::history::{AgentId, UserId};
use crate::messagebus::{AgentEvent, AgentForked, MessageBus};
use crate::rpc::{
    AgentEventNotification, AgentForkedNotification, PristineRpcServer, RpcServerImpl,
};

#[derive(Debug)]
pub enum StdioError {
    Io(std::io::Error),
    InvalidRequest(String),
    Bus(crate::messagebus::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for StdioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StdioError::Io(e) => write!(f, "IO: {e}"),
            StdioError::InvalidRequest(msg) => write!(f, "invalid JSON-RPC request: {msg}"),
            StdioError::Bus(e) => write!(f, "bus: {e}"),
            StdioError::Json(e) => write!(f, "JSON: {e}"),
        }
    }
}

impl std::error::Error for StdioError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StdioError::Io(e) => Some(e),
            StdioError::InvalidRequest(_) => None,
            StdioError::Bus(e) => Some(e),
            StdioError::Json(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for StdioError {
    fn from(e: std::io::Error) -> Self {
        StdioError::Io(e)
    }
}

impl From<crate::messagebus::Error> for StdioError {
    fn from(e: crate::messagebus::Error) -> Self {
        StdioError::Bus(e)
    }
}

impl From<serde_json::Error> for StdioError {
    fn from(e: serde_json::Error) -> Self {
        StdioError::Json(e)
    }
}

fn spawn_stdin_reader() -> tokio::sync::mpsc::Receiver<String> {
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(16);
    std::thread::spawn(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin().lock();
        let reader = std::io::BufReader::new(stdin);
        for line in reader.lines() {
            match line {
                Ok(l) if l.trim().is_empty() => continue,
                Ok(l) => {
                    if tx.blocking_send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

pub async fn run_server(
    bus: Arc<dyn MessageBus>,
    agent_id: AgentId,
    owner_id: UserId,
    shutdown_token: CancellationToken,
    skills: Option<Arc<SkillsAnnouncer>>,
) -> Result<(), StdioError> {
    let server = RpcServerImpl::new(bus.clone(), agent_id, owner_id, shutdown_token.clone());
    let module = server.into_rpc();
    let mut stdout = tokio::io::stdout();
    let mut lines = spawn_stdin_reader();

    loop {
        tokio::select! {
            _ = shutdown_token.cancelled() => break,
            line = lines.recv() => {
                let Some(text) = line else { break };

                let envelope: RequestEnvelope = serde_json::from_str(&text)?;

                let (response, _rx) = module
                    .raw_json_request(&text, 1)
                    .await
                    .map_err(|e| StdioError::InvalidRequest(e.to_string()))?;

                let success = serde_json::from_str::<ResponseEnvelope>(response.get())
                    .ok()
                    .and_then(|r| r.result)
                    .is_some();
                write_line(&mut stdout, response.get()).await?;

                if let DispatchOutcome::DrainEvents =
                    classify_outcome(&envelope.method, success)
                {
                    // The first turn's system-prompt render triggers skills
                    // discovery; surface its outcome once before draining the
                    // agent's event stream. `SkillsAnnouncer::take` is a no-op
                    // on every subsequent turn.
                    if let Some(announcer) = &skills {
                        write_skills_notifications(announcer, &mut stdout).await?;
                    }
                    drain_events(&*bus, agent_id, &mut stdout).await?;
                }
            }
        }
    }
    Ok(())
}

async fn write_skills_notifications<W>(
    announcer: &SkillsAnnouncer,
    out: &mut W,
) -> Result<(), StdioError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    for notification in announcer.take() {
        let jsonrpc = serde_json::json!({
            "jsonrpc": "2.0",
            "method": notification.method(),
            "params": notification.params()?,
        });
        write_line(out, &jsonrpc.to_string()).await?;
    }
    Ok(())
}

async fn write_agent_forked<W>(out: &mut W, event: &AgentForked) -> Result<(), StdioError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let notification = AgentForkedNotification::from_event(event);
    let jsonrpc = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "agent_forked",
        "params": notification,
    });
    write_line(out, &jsonrpc.to_string()).await?;
    Ok(())
}

#[derive(serde::Deserialize)]
struct RequestEnvelope {
    method: String,
}

#[derive(serde::Deserialize)]
struct ResponseEnvelope {
    result: Option<serde_json::Value>,
}

enum DispatchOutcome {
    Done,
    DrainEvents,
}

fn classify_outcome(method: &str, success: bool) -> DispatchOutcome {
    match (method, success) {
        ("send_message", true) => DispatchOutcome::DrainEvents,
        _ => DispatchOutcome::Done,
    }
}

async fn write_line<W>(out: &mut W, text: &str) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    out.write_all(text.as_bytes()).await?;
    out.write_all(b"\n").await?;
    out.flush().await?;
    Ok(())
}

/// Forward a turn's agent events to the client until every active agent is idle.
///
/// A turn does not end when the messaged agent idles: it may have `fork`ed a
/// peer that is still working, and that peer's events are published under its
/// own id. We subscribe to the fork broadcast, merge each new peer's event
/// stream in as it appears, and keep forwarding (each event tagged with its
/// originating agent id) until the messaged agent and every forked peer have
/// gone idle. `biased` polling guarantees a peer's `agent_forked` is registered
/// before the messaged agent's `Idle` can empty the active set.
async fn drain_events<W>(
    bus: &dyn MessageBus,
    agent_id: AgentId,
    out: &mut W,
) -> Result<(), StdioError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut agent_events: SelectAll<BoxStream<'static, (AgentId, AgentEvent)>> = SelectAll::new();
    agent_events.push(bus.subscribe(agent_id)?.map(move |e| (agent_id, e)).boxed());
    let mut forks = bus.subscribe_forks();
    let mut active: HashSet<AgentId> = HashSet::from([agent_id]);

    while !active.is_empty() {
        tokio::select! {
            biased;
            Some(forked) = forks.next() => {
                write_agent_forked(out, &forked).await?;
                if active.insert(forked.agent_id) {
                    let fid = forked.agent_id;
                    agent_events.push(bus.subscribe(fid)?.map(move |e| (fid, e)).boxed());
                }
            }
            Some((id, event)) = agent_events.next() => {
                let is_terminal = matches!(event, AgentEvent::Idle | AgentEvent::Error { .. });
                let notification = AgentEventNotification::from_event(id, &event);
                let jsonrpc = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "agent.event",
                    "params": notification,
                });
                write_line(out, &jsonrpc.to_string()).await?;
                if is_terminal {
                    active.remove(&id);
                }
            }
            else => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::ResolvedSkillsConfig;
    use crate::skills::SkillsRegistry;
    use crate::test_support::SkillsFixture;

    /// Parse the writer's bytes as newline-delimited JSON-RPC notifications.
    fn parse_notifications(
        bytes: &[u8],
    ) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
        let text = std::str::from_utf8(bytes)?;
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<serde_json::Value>(l).map_err(Into::into))
            .collect()
    }

    /// Exercises the real write path end-to-end: a real `SkillsRegistry`
    /// scanning a `SkillsFixture` (one valid skill, one malformed) wrapped in a
    /// `SkillsAnnouncer`, drained through the (now generic) writer helper into an
    /// in-memory buffer, then parsed back as JSON-RPC. This closes the gap that
    /// the notification path was only unit-tested at the announcer level, never
    /// through `write_skills_notifications` / `write_line`.
    #[tokio::test]
    async fn write_skills_notifications_serializes_loaded_and_diagnostics_over_writer()
    -> Result<(), Box<dyn std::error::Error>> {
        // A valid skill (well-formed frontmatter) yields a catalog entry; a
        // malformed one (frontmatter omits `description`) yields a diagnostic.
        let fixture = SkillsFixture::new()?
            .add_skill("alpha", "first skill", "body text")?
            .add_raw_skill("broken", "---\nname: broken\n---\nbody\n")?;
        let path = fixture
            .path()
            .to_str()
            .ok_or("fixture path not valid UTF-8")?
            .to_string();
        let config = ResolvedSkillsConfig {
            user_paths: Some(vec![path]),
            project_paths: Some(vec![]),
            disabled: vec![],
        };
        let source: Arc<dyn crate::skills::SkillsRegistrySource> =
            Arc::new(SkillsRegistry::new(config, false));
        let announcer = SkillsAnnouncer::new(source);

        let mut buf: Vec<u8> = Vec::new();
        write_skills_notifications(&announcer, &mut buf).await?;

        let notifications = parse_notifications(&buf)?;
        assert!(
            !notifications.is_empty(),
            "expected at least one notification written"
        );

        // Every line is a well-formed JSON-RPC notification: version "2.0", a
        // method, params, and no id.
        for n in &notifications {
            assert_eq!(n["jsonrpc"], "2.0", "missing jsonrpc 2.0: {n}");
            assert!(n.get("method").is_some(), "missing method: {n}");
            assert!(n.get("params").is_some(), "missing params: {n}");
            assert!(
                n.get("id").is_none(),
                "notification must not carry an id: {n}"
            );
        }

        // skills_loaded carries the valid skill's {name, description}.
        let loaded = notifications
            .iter()
            .find(|n| n["method"] == "skills_loaded")
            .ok_or("expected a skills_loaded notification")?;
        let skills = loaded["params"]["skills"]
            .as_array()
            .ok_or("skills_loaded params.skills must be an array")?;
        assert!(
            skills
                .iter()
                .any(|s| s["name"] == "alpha" && s["description"] == "first skill"),
            "skills_loaded missing valid skill entry: {loaded}"
        );

        // skills_diagnostics is a kind-tagged array containing the malformed
        // skill's entry.
        let diagnostics = notifications
            .iter()
            .find(|n| n["method"] == "skills_diagnostics")
            .ok_or("expected a skills_diagnostics notification")?;
        let entries = diagnostics["params"]
            .as_array()
            .ok_or("skills_diagnostics params must be an array")?;
        assert!(!entries.is_empty(), "expected at least one diagnostic");
        assert!(
            entries.iter().all(|e| e.get("kind").is_some()),
            "every diagnostic must carry a kind tag: {diagnostics}"
        );
        assert!(
            entries.iter().any(|e| e["kind"] == "description_missing"),
            "expected the malformed skill's description_missing diagnostic: {diagnostics}"
        );

        // Emit-once: a second drain on the same announcer writes nothing.
        let mut buf2: Vec<u8> = Vec::new();
        write_skills_notifications(&announcer, &mut buf2).await?;
        assert!(
            buf2.is_empty(),
            "second drain must write nothing (emit-once guard), got: {buf2:?}"
        );

        Ok(())
    }

    /// Exercises the fork notification write path: an [`AgentForked`] bus event
    /// serializes to a well-formed `agent_forked` JSON-RPC notification carrying
    /// the new agent id, the origin, and the inherited handle.
    #[tokio::test]
    async fn write_agent_forked_serializes_notification_over_writer()
    -> Result<(), Box<dyn std::error::Error>> {
        let event = AgentForked {
            agent_id: AgentId::new(),
            origin: AgentId::new(),
            handle: crate::history::CheckpointHandle::genesis(),
        };

        let mut buf: Vec<u8> = Vec::new();
        write_agent_forked(&mut buf, &event).await?;

        let notifications = parse_notifications(&buf)?;
        let n = notifications.first().ok_or("expected one notification")?;
        assert_eq!(n["jsonrpc"], "2.0");
        assert_eq!(n["method"], "agent_forked");
        assert!(n.get("id").is_none(), "notification must not carry an id");
        assert_eq!(n["params"]["agent_id"], event.agent_id.to_string());
        assert_eq!(n["params"]["origin"], event.origin.to_string());
        assert_eq!(n["params"]["handle"], event.handle.to_string());
        Ok(())
    }
}
