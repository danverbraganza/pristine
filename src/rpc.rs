//! JSON-RPC 2.0 server trait and implementation.

use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::history::{AgentId, Block, UserId};
use crate::messagebus::{AgentEvent, MessageBus};
use crate::model::Usage;
use crate::skills::{SkillDiagnostic, SkillSummary};

#[derive(Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    pub agent_id: AgentId,
    pub owner_id: UserId,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SendMessageResult {
    pub ok: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ShutdownResult {
    pub ok: bool,
}

#[derive(Serialize)]
pub struct AgentEventNotification {
    pub agent_id: AgentId,
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: serde_json::Value,
}

impl AgentEventNotification {
    pub fn from_event(agent_id: AgentId, event: &AgentEvent) -> Self {
        match event {
            AgentEvent::TokenDelta { text } => Self {
                agent_id,
                event_type: "token_delta".to_string(),
                data: serde_json::json!({ "text": text }),
            },
            AgentEvent::ReasoningDelta { text } => Self {
                agent_id,
                event_type: "reasoning_delta".to_string(),
                data: serde_json::json!({ "text": text }),
            },
            AgentEvent::BlockComplete { block } => Self {
                agent_id,
                event_type: "block_complete".to_string(),
                data: block_to_value(block.block()),
            },
            AgentEvent::RunComplete { usage } => Self {
                agent_id,
                event_type: "run_complete".to_string(),
                data: usage_to_value(usage),
            },
            AgentEvent::Error { message } => Self {
                agent_id,
                event_type: "error".to_string(),
                data: serde_json::json!({ "message": message }),
            },
            AgentEvent::Idle => Self {
                agent_id,
                event_type: "idle".to_string(),
                data: serde_json::json!({}),
            },
        }
    }
}

/// Params for the `skills_loaded` notification: the discovered catalog
/// projected to tier-1 `{ name, description }` summaries.
#[derive(Serialize)]
pub struct SkillsLoadedNotification {
    pub skills: Vec<SkillSummary>,
}

/// A session-level skills notification fired once after the first system-prompt
/// render triggers discovery. Unlike [`AgentEventNotification`] these are not
/// agent-keyed: they describe the engine's global skills catalog.
///
/// Each variant maps to a distinct JSON-RPC method name
/// ([`method`](SkillsNotification::method)) carrying its own `params` shape,
/// mirroring the `agent.event` method + params dispatch in
/// [`crate::stdio`].
pub enum SkillsNotification {
    /// `skills_loaded` — the catalog `{ skills: [{ name, description }, ...] }`.
    Loaded(SkillsLoadedNotification),
    /// `skills_diagnostics` — the array of kind-tagged diagnostics.
    Diagnostics(Vec<SkillDiagnostic>),
}

impl SkillsNotification {
    /// The JSON-RPC method name carried by this notification.
    pub fn method(&self) -> &'static str {
        match self {
            SkillsNotification::Loaded(_) => "skills_loaded",
            SkillsNotification::Diagnostics(_) => "skills_diagnostics",
        }
    }

    /// The JSON-RPC `params` value for this notification.
    pub fn params(&self) -> Result<serde_json::Value, serde_json::Error> {
        match self {
            SkillsNotification::Loaded(loaded) => serde_json::to_value(loaded),
            SkillsNotification::Diagnostics(diagnostics) => serde_json::to_value(diagnostics),
        }
    }
}

fn usage_to_value(usage: &Usage) -> serde_json::Value {
    serde_json::json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
    })
}

fn block_to_value(block: &Block) -> serde_json::Value {
    match block {
        Block::UserMessage { from, content, .. } => serde_json::json!({
            "block_type": "user_message",
            "from": from,
            "content": content,
        }),
        Block::AgentMessage { from, content, .. } => serde_json::json!({
            "block_type": "agent_message",
            "from": from,
            "content": content,
        }),
        Block::ToolCall {
            id,
            name,
            arguments,
            ..
        } => serde_json::json!({
            "block_type": "tool_call",
            "id": id,
            "name": name,
            "arguments": arguments,
        }),
        Block::ToolResult {
            tool_use_id,
            name,
            result,
            is_error,
            ..
        } => serde_json::json!({
            "block_type": "tool_result",
            "tool_use_id": tool_use_id,
            "name": name,
            "result": result,
            "is_error": is_error,
        }),
        Block::ReasoningTrace { content, .. } => serde_json::json!({
            "block_type": "reasoning_trace",
            "content": content,
        }),
    }
}

#[jsonrpsee::proc_macros::rpc(server)]
pub trait PristineRpc {
    #[method(name = "initialize")]
    async fn initialize(&self) -> Result<InitializeResult, jsonrpsee::types::ErrorObjectOwned>;

    #[method(name = "send_message", param_kind = map)]
    async fn send_message(
        &self,
        agent_id: AgentId,
        content: String,
    ) -> Result<SendMessageResult, jsonrpsee::types::ErrorObjectOwned>;

    #[method(name = "shutdown")]
    async fn shutdown(&self) -> Result<ShutdownResult, jsonrpsee::types::ErrorObjectOwned>;
}

pub struct RpcServerImpl {
    bus: Arc<dyn MessageBus>,
    agent_id: AgentId,
    owner_id: UserId,
    shutdown_token: CancellationToken,
}

impl RpcServerImpl {
    pub fn new(
        bus: Arc<dyn MessageBus>,
        agent_id: AgentId,
        owner_id: UserId,
        shutdown_token: CancellationToken,
    ) -> Self {
        Self {
            bus,
            agent_id,
            owner_id,
            shutdown_token,
        }
    }
}

#[jsonrpsee::core::async_trait]
impl PristineRpcServer for RpcServerImpl {
    async fn initialize(&self) -> Result<InitializeResult, jsonrpsee::types::ErrorObjectOwned> {
        Ok(InitializeResult {
            agent_id: self.agent_id,
            owner_id: self.owner_id,
        })
    }

    async fn send_message(
        &self,
        agent_id: AgentId,
        content: String,
    ) -> Result<SendMessageResult, jsonrpsee::types::ErrorObjectOwned> {
        let block = Block::UserMessage {
            from: self.owner_id,
            content,
            timestamp: SystemTime::now(),
        };
        self.bus.send_inbound(agent_id, block).map_err(|e| {
            jsonrpsee::types::ErrorObject::owned(
                jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                e.to_string(),
                None::<()>,
            )
        })?;
        Ok(SendMessageResult { ok: true })
    }

    async fn shutdown(&self) -> Result<ShutdownResult, jsonrpsee::types::ErrorObjectOwned> {
        self.shutdown_token.cancel();
        Ok(ShutdownResult { ok: true })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messagebus::InMemoryMessageBus;
    use jsonrpsee::RpcModule;

    fn build_rpc_module() -> (
        RpcModule<RpcServerImpl>,
        AgentId,
        futures::stream::BoxStream<'static, Block>,
    ) {
        let bus: Arc<dyn MessageBus> = Arc::new(InMemoryMessageBus::new());
        let agent_id = AgentId::new();
        let owner_id = UserId::new();
        let token = CancellationToken::new();

        let inbound = bus.register(agent_id).expect("register agent");

        let server = RpcServerImpl::new(bus, agent_id, owner_id, token);
        (server.into_rpc(), agent_id, inbound)
    }

    #[tokio::test]
    async fn initialize_returns_ids() {
        let (module, agent_id, _inbound) = build_rpc_module();
        let response: InitializeResult = module
            .call("initialize", jsonrpsee::core::params::ObjectParams::new())
            .await
            .expect("initialize call");
        assert_eq!(response.agent_id, agent_id);
    }

    #[tokio::test]
    async fn send_message_returns_ok() {
        let (module, agent_id, _inbound) = build_rpc_module();
        let mut params = jsonrpsee::core::params::ObjectParams::new();
        params
            .insert("agent_id", agent_id)
            .expect("insert agent_id");
        params.insert("content", "hello").expect("insert content");
        let response: SendMessageResult = module
            .call("send_message", params)
            .await
            .expect("send_message call");
        assert!(response.ok);
    }

    #[tokio::test]
    async fn shutdown_cancels_token() {
        let bus: Arc<dyn MessageBus> = Arc::new(InMemoryMessageBus::new());
        let agent_id = AgentId::new();
        let owner_id = UserId::new();
        let token = CancellationToken::new();
        let token_clone = token.clone();

        let _inbound = bus.register(agent_id).expect("register agent");

        let server = RpcServerImpl::new(bus, agent_id, owner_id, token);
        let module: RpcModule<RpcServerImpl> = server.into_rpc();

        assert!(!token_clone.is_cancelled());
        let response: ShutdownResult = module
            .call("shutdown", jsonrpsee::core::params::ObjectParams::new())
            .await
            .expect("shutdown call");
        assert!(response.ok);
        assert!(token_clone.is_cancelled());
    }

    #[tokio::test]
    async fn send_message_to_unknown_agent_returns_error() {
        let bus: Arc<dyn MessageBus> = Arc::new(InMemoryMessageBus::new());
        let registered_id = AgentId::new();
        let unknown_id = AgentId::new();
        let owner_id = UserId::new();
        let token = CancellationToken::new();

        let _inbound = bus.register(registered_id).expect("register agent");

        let server = RpcServerImpl::new(bus, registered_id, owner_id, token);
        let module: RpcModule<RpcServerImpl> = server.into_rpc();

        let mut params = jsonrpsee::core::params::ObjectParams::new();
        params
            .insert("agent_id", unknown_id)
            .expect("insert agent_id");
        params.insert("content", "hello").expect("insert content");
        let result: Result<SendMessageResult, _> = module.call("send_message", params).await;
        assert!(result.is_err());
    }

    #[test]
    fn agent_event_notification_token_delta() {
        let agent_id = AgentId::new();
        let event = AgentEvent::TokenDelta {
            text: "hello".to_string(),
        };
        let notification = AgentEventNotification::from_event(agent_id, &event);
        assert_eq!(notification.event_type, "token_delta");
        assert_eq!(notification.data["text"], "hello");
    }

    #[test]
    fn agent_event_notification_reasoning_delta() {
        let agent_id = AgentId::new();
        let event = AgentEvent::ReasoningDelta {
            text: "thinking".to_string(),
        };
        let notification = AgentEventNotification::from_event(agent_id, &event);
        assert_eq!(notification.event_type, "reasoning_delta");
        assert_eq!(notification.data["text"], "thinking");
    }

    #[test]
    fn block_complete_notification_user_message_shape() {
        let agent_id = AgentId::new();
        let event = AgentEvent::BlockComplete {
            block: Arc::new(crate::history::HistoryNode::new(
                crate::history::NodeId::new(),
                Block::UserMessage {
                    from: UserId::new(),
                    content: "hello".to_string(),
                    timestamp: SystemTime::now(),
                },
                None,
            )),
        };
        let notification = AgentEventNotification::from_event(agent_id, &event);
        assert_eq!(notification.event_type, "block_complete");
        assert_eq!(notification.data["block_type"], "user_message");
        assert_eq!(notification.data["content"], "hello");
        assert!(
            notification.data["from"]
                .as_str()
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
                .is_some()
        );
    }

    #[test]
    fn block_complete_notification_agent_message_shape() {
        let agent_id = AgentId::new();
        let event = AgentEvent::BlockComplete {
            block: Arc::new(crate::history::HistoryNode::new(
                crate::history::NodeId::new(),
                Block::AgentMessage {
                    from: AgentId::new(),
                    content: "world".to_string(),
                    timestamp: SystemTime::now(),
                },
                None,
            )),
        };
        let notification = AgentEventNotification::from_event(agent_id, &event);
        assert_eq!(notification.event_type, "block_complete");
        assert_eq!(notification.data["block_type"], "agent_message");
        assert_eq!(notification.data["content"], "world");
        assert!(
            notification.data["from"]
                .as_str()
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
                .is_some()
        );
    }

    #[test]
    fn block_complete_notification_tool_call_shape() {
        let agent_id = AgentId::new();
        let event = AgentEvent::BlockComplete {
            block: Arc::new(crate::history::HistoryNode::new(
                crate::history::NodeId::new(),
                Block::ToolCall {
                    id: "use_42".to_string(),
                    name: "echo".to_string(),
                    arguments: serde_json::json!({"k": "v"}),
                    timestamp: SystemTime::now(),
                },
                None,
            )),
        };
        let notification = AgentEventNotification::from_event(agent_id, &event);
        assert_eq!(notification.event_type, "block_complete");
        assert_eq!(notification.data["block_type"], "tool_call");
        assert_eq!(notification.data["id"], "use_42");
        assert_eq!(notification.data["name"], "echo");
        assert_eq!(notification.data["arguments"]["k"], "v");
    }

    #[test]
    fn block_complete_notification_tool_result_shape() {
        let agent_id = AgentId::new();
        let event = AgentEvent::BlockComplete {
            block: Arc::new(crate::history::HistoryNode::new(
                crate::history::NodeId::new(),
                Block::ToolResult {
                    tool_use_id: "use_42".to_string(),
                    name: "echo".to_string(),
                    result: serde_json::json!({"echo": {"k": "v"}}),
                    is_error: false,
                    timestamp: SystemTime::now(),
                },
                None,
            )),
        };
        let notification = AgentEventNotification::from_event(agent_id, &event);
        assert_eq!(notification.event_type, "block_complete");
        assert_eq!(notification.data["block_type"], "tool_result");
        assert_eq!(notification.data["tool_use_id"], "use_42");
        assert_eq!(notification.data["name"], "echo");
        assert_eq!(notification.data["result"]["echo"]["k"], "v");
        assert_eq!(notification.data["is_error"], false);
    }

    #[test]
    fn block_complete_notification_reasoning_trace_shape() {
        let agent_id = AgentId::new();
        let event = AgentEvent::BlockComplete {
            block: Arc::new(crate::history::HistoryNode::new(
                crate::history::NodeId::new(),
                Block::ReasoningTrace {
                    content: "thinking...".to_string(),
                    timestamp: SystemTime::now(),
                },
                None,
            )),
        };
        let notification = AgentEventNotification::from_event(agent_id, &event);
        assert_eq!(notification.event_type, "block_complete");
        assert_eq!(notification.data["block_type"], "reasoning_trace");
        assert_eq!(notification.data["content"], "thinking...");
    }

    #[test]
    fn agent_event_notification_run_complete() {
        let agent_id = AgentId::new();
        let event = AgentEvent::RunComplete {
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
            },
        };
        let notification = AgentEventNotification::from_event(agent_id, &event);
        assert_eq!(notification.event_type, "run_complete");
        assert_eq!(notification.data["input_tokens"], 100);
        assert_eq!(notification.data["output_tokens"], 50);
    }

    #[test]
    fn agent_event_notification_error() {
        let agent_id = AgentId::new();
        let event = AgentEvent::Error {
            message: "boom".to_string(),
        };
        let notification = AgentEventNotification::from_event(agent_id, &event);
        assert_eq!(notification.event_type, "error");
        assert_eq!(notification.data["message"], "boom");
    }

    #[test]
    fn skills_loaded_notification_envelope_shape() -> Result<(), serde_json::Error> {
        let notification = SkillsNotification::Loaded(SkillsLoadedNotification {
            skills: vec![SkillSummary {
                name: "alpha".to_string(),
                description: "first".to_string(),
            }],
        });
        assert_eq!(notification.method(), "skills_loaded");
        let jsonrpc = serde_json::json!({
            "jsonrpc": "2.0",
            "method": notification.method(),
            "params": notification.params()?,
        });
        assert_eq!(jsonrpc["method"], "skills_loaded");
        assert!(jsonrpc.get("id").is_none(), "notifications carry no id");
        assert_eq!(jsonrpc["params"]["skills"][0]["name"], "alpha");
        assert_eq!(jsonrpc["params"]["skills"][0]["description"], "first");
        Ok(())
    }

    #[test]
    fn skills_diagnostics_notification_envelope_shape() -> Result<(), serde_json::Error> {
        let notification =
            SkillsNotification::Diagnostics(vec![SkillDiagnostic::DescriptionMissing {
                path: std::path::PathBuf::from("/tmp/x/SKILL.md"),
            }]);
        assert_eq!(notification.method(), "skills_diagnostics");
        let jsonrpc = serde_json::json!({
            "jsonrpc": "2.0",
            "method": notification.method(),
            "params": notification.params()?,
        });
        assert_eq!(jsonrpc["method"], "skills_diagnostics");
        assert!(jsonrpc.get("id").is_none(), "notifications carry no id");
        assert!(
            jsonrpc["params"].is_array(),
            "diagnostics params is an array"
        );
        assert_eq!(jsonrpc["params"][0]["kind"], "description_missing");
        Ok(())
    }

    #[test]
    fn agent_event_notification_idle() {
        let agent_id = AgentId::new();
        let event = AgentEvent::Idle;
        let notification = AgentEventNotification::from_event(agent_id, &event);
        assert_eq!(notification.event_type, "idle");
        assert!(
            notification
                .data
                .as_object()
                .map(|o| o.is_empty())
                .unwrap_or(false)
        );
    }
}
