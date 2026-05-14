use std::sync::Arc;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use pristine::history::{AgentId, UserId};
use pristine::messagebus::{AgentEvent, InMemoryMessageBus, MessageBus};
use pristine::model::Usage;
use pristine::rpc::{AgentEventNotification, PristineRpcServer, RpcServerImpl};

fn build_rpc_module() -> (
    jsonrpsee::RpcModule<RpcServerImpl>,
    AgentId,
    UserId,
    CancellationToken,
    Arc<InMemoryMessageBus>,
    futures::stream::BoxStream<'static, pristine::history::Block>,
) {
    let bus = Arc::new(InMemoryMessageBus::new());
    let agent_id = AgentId::new();
    let owner_id = UserId::new();
    let token = CancellationToken::new();

    let inbound = bus.register(agent_id).expect("register agent");

    let server = RpcServerImpl::new(bus.clone(), agent_id, owner_id, token.clone());
    (server.into_rpc(), agent_id, owner_id, token, bus, inbound)
}

#[tokio::test]
async fn initialize_via_raw_json_rpc() {
    let (module, agent_id, owner_id, _token, _bus, _inbound) = build_rpc_module();

    let request = r#"{"jsonrpc": "2.0", "method": "initialize", "id": 1}"#;
    let (response, _rx) = module
        .raw_json_request(request, 1)
        .await
        .expect("raw_json_request should parse valid JSON-RPC");

    let value: serde_json::Value =
        serde_json::from_str(response.get()).expect("response should be valid JSON");

    assert_eq!(value["jsonrpc"], "2.0");
    assert_eq!(value["id"], 1);

    let result = &value["result"];
    assert_eq!(
        result["agent_id"],
        serde_json::to_value(agent_id).expect("serialize agent_id")
    );
    assert_eq!(
        result["owner_id"],
        serde_json::to_value(owner_id).expect("serialize owner_id")
    );
}

#[tokio::test]
async fn send_message_via_raw_json_rpc_routes_to_inbound() {
    let (module, agent_id, _owner_id, _token, _bus, mut inbound) = build_rpc_module();

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "send_message",
        "id": 2,
        "params": {
            "agent_id": agent_id,
            "content": "hello from test"
        }
    })
    .to_string();

    let (response, _rx) = module
        .raw_json_request(&request, 1)
        .await
        .expect("raw_json_request should parse valid JSON-RPC");

    let value: serde_json::Value =
        serde_json::from_str(response.get()).expect("response should be valid JSON");

    assert_eq!(value["jsonrpc"], "2.0");
    assert_eq!(value["id"], 2);
    assert_eq!(value["result"]["ok"], true);

    let block = tokio::time::timeout(std::time::Duration::from_secs(1), inbound.next())
        .await
        .expect("timed out waiting for inbound block")
        .expect("inbound stream closed");

    match block {
        pristine::history::Block::UserMessage { content, .. } => {
            assert_eq!(content, "hello from test");
        }
        other => panic!("expected UserMessage, got {other:?}"),
    }
}

#[tokio::test]
async fn shutdown_via_raw_json_rpc_cancels_token() {
    let (module, _agent_id, _owner_id, token, _bus, _inbound) = build_rpc_module();

    assert!(!token.is_cancelled());

    let request = r#"{"jsonrpc": "2.0", "method": "shutdown", "id": 3}"#;
    let (response, _rx) = module
        .raw_json_request(request, 1)
        .await
        .expect("raw_json_request should parse valid JSON-RPC");

    let value: serde_json::Value =
        serde_json::from_str(response.get()).expect("response should be valid JSON");

    assert_eq!(value["jsonrpc"], "2.0");
    assert_eq!(value["id"], 3);
    assert_eq!(value["result"]["ok"], true);
    assert!(token.is_cancelled());
}

#[tokio::test]
async fn event_notification_serializes_as_json_rpc() {
    let agent_id = AgentId::new();

    let events = vec![
        (
            AgentEvent::TokenDelta {
                text: "hello".to_string(),
            },
            "token_delta",
        ),
        (
            AgentEvent::RunComplete {
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            },
            "run_complete",
        ),
        (
            AgentEvent::Error {
                message: "something broke".to_string(),
            },
            "error",
        ),
    ];

    for (event, expected_type) in events {
        let notification = AgentEventNotification::from_event(agent_id, &event);
        let jsonrpc = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "agent.event",
            "params": notification,
        });

        assert_eq!(jsonrpc["jsonrpc"], "2.0");
        assert_eq!(jsonrpc["method"], "agent.event");
        assert!(
            jsonrpc.get("id").is_none(),
            "notifications must not have an id"
        );
        assert_eq!(jsonrpc["params"]["type"], expected_type);
        assert_eq!(
            jsonrpc["params"]["agent_id"],
            serde_json::to_value(agent_id).expect("serialize agent_id")
        );

        match &event {
            AgentEvent::TokenDelta { text } => {
                assert_eq!(jsonrpc["params"]["data"]["text"], text.as_str());
            }
            AgentEvent::RunComplete { usage } => {
                assert_eq!(
                    jsonrpc["params"]["data"]["input_tokens"],
                    usage.input_tokens
                );
                assert_eq!(
                    jsonrpc["params"]["data"]["output_tokens"],
                    usage.output_tokens
                );
            }
            AgentEvent::Error { message } => {
                assert_eq!(jsonrpc["params"]["data"]["message"], message.as_str());
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn invalid_method_returns_json_rpc_error() {
    let (module, _agent_id, _owner_id, _token, _bus, _inbound) = build_rpc_module();

    let request = r#"{"jsonrpc": "2.0", "method": "nonexistent_method", "id": 99}"#;
    let (response, _rx) = module
        .raw_json_request(request, 1)
        .await
        .expect("raw_json_request should parse valid JSON-RPC");

    let value: serde_json::Value =
        serde_json::from_str(response.get()).expect("response should be valid JSON");

    assert_eq!(value["jsonrpc"], "2.0");
    assert_eq!(value["id"], 99);
    assert!(
        value.get("error").is_some(),
        "expected error field in response for unknown method"
    );
    assert!(
        value.get("result").is_none(),
        "should not have result field on error response"
    );
}
