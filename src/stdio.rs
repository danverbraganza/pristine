//! Stdio transport for JSON-RPC dispatch.

use std::sync::Arc;

use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::history::{AgentId, UserId};
use crate::messagebus::{AgentEvent, InMemoryMessageBus, MessageBus};
use crate::rpc::{AgentEventNotification, PristineRpcServer, RpcServerImpl};

pub async fn run_server(
    bus: Arc<InMemoryMessageBus>,
    agent_id: AgentId,
    owner_id: UserId,
    shutdown_token: CancellationToken,
) -> anyhow::Result<()> {
    let server = RpcServerImpl::new(bus.clone(), agent_id, owner_id, shutdown_token.clone());
    let module = server.into_rpc();

    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    loop {
        tokio::select! {
            _ = shutdown_token.cancelled() => break,
            line = lines.next_line() => {
                match line? {
                    Some(ref text) if text.trim().is_empty() => continue,
                    Some(text) => {
                        let is_send = is_method(&text, "send_message");

                        let (response, _rx) = module
                            .raw_json_request(&text, 1)
                            .await
                            .map_err(|e| anyhow::anyhow!("invalid JSON-RPC request: {e}"))?;

                        write_line(&stdout, response.get()).await?;

                        if is_send && is_success(response.get()) {
                            drain_events(&bus, agent_id, &stdout).await?;
                        }
                    }
                    None => break,
                }
            }
        }
    }
    Ok(())
}

fn is_method(line: &str, method: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("method")?.as_str().map(|m| m == method))
        .unwrap_or(false)
}

fn is_success(response: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(response)
        .ok()
        .map(|v| v.get("result").is_some())
        .unwrap_or(false)
}

async fn write_line(stdout: &Arc<Mutex<tokio::io::Stdout>>, text: &str) -> anyhow::Result<()> {
    let mut out = stdout.lock().await;
    out.write_all(text.as_bytes()).await?;
    out.write_all(b"\n").await?;
    out.flush().await?;
    Ok(())
}

async fn drain_events(
    bus: &Arc<InMemoryMessageBus>,
    agent_id: AgentId,
    stdout: &Arc<Mutex<tokio::io::Stdout>>,
) -> anyhow::Result<()> {
    let mut events = bus
        .subscribe(agent_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
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
