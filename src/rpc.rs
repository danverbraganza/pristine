//! JSON-RPC 2.0 server trait and implementation.

use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::history::{AgentId, Block, UserId};
use crate::messagebus::{AgentEvent, InMemoryMessageBus, MessageBus};
use crate::model::Usage;

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
            AgentEvent::BlockComplete { .. } => Self {
                agent_id,
                event_type: "block_complete".to_string(),
                data: serde_json::json!({}),
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
        }
    }
}

fn usage_to_value(usage: &Usage) -> serde_json::Value {
    serde_json::json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
    })
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
    bus: Arc<InMemoryMessageBus>,
    agent_id: AgentId,
    owner_id: UserId,
    shutdown_token: CancellationToken,
}

impl RpcServerImpl {
    pub fn new(
        bus: Arc<InMemoryMessageBus>,
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
    use jsonrpsee::RpcModule;

    fn build_rpc_module() -> (
        RpcModule<RpcServerImpl>,
        AgentId,
        futures::stream::BoxStream<'static, Block>,
    ) {
        let bus = Arc::new(InMemoryMessageBus::new());
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
        let bus = Arc::new(InMemoryMessageBus::new());
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
        let bus = Arc::new(InMemoryMessageBus::new());
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
    fn agent_event_notification_block_complete() {
        let agent_id = AgentId::new();
        let event = AgentEvent::BlockComplete {
            block: Arc::new(crate::history::HistoryNode::new(
                crate::history::NodeId::new(),
                Block::UserMessage {
                    from: UserId::new(),
                    content: "test".to_string(),
                    timestamp: SystemTime::now(),
                },
                None,
            )),
        };
        let notification = AgentEventNotification::from_event(agent_id, &event);
        assert_eq!(notification.event_type, "block_complete");
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
}
