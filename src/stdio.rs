//! Stdio transport for JSON-RPC dispatch.

use std::sync::Arc;

use futures::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::history::{AgentId, UserId};
use crate::messagebus::{AgentEvent, MessageBus};
use crate::rpc::{AgentEventNotification, PristineRpcServer, RpcServerImpl};

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
                    drain_events(&*bus, agent_id, &mut stdout).await?;
                }
            }
        }
    }
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

async fn write_line(out: &mut tokio::io::Stdout, text: &str) -> std::io::Result<()> {
    out.write_all(text.as_bytes()).await?;
    out.write_all(b"\n").await?;
    out.flush().await?;
    Ok(())
}

async fn drain_events(
    bus: &dyn MessageBus,
    agent_id: AgentId,
    stdout: &mut tokio::io::Stdout,
) -> Result<(), StdioError> {
    let mut events = bus.subscribe(agent_id)?;
    while let Some(event) = events.next().await {
        let is_terminal = matches!(
            event,
            AgentEvent::RunComplete { .. } | AgentEvent::Error { .. }
        );
        let notification = AgentEventNotification::from_event(agent_id, &event);
        let jsonrpc = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "agent.event",
            "params": notification,
        });
        write_line(stdout, &jsonrpc.to_string()).await?;
        if is_terminal {
            break;
        }
    }
    Ok(())
}
