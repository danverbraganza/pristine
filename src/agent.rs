//! Agent and event loop.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use futures::StreamExt;
use futures::stream::BoxStream;

pub use crate::history::AgentId;
use crate::history::{Block, History};
use crate::messagebus::{self, AgentEvent, MessageBus};
use crate::model::{
    self, ARModel, ContentPart, ModelInput, ModelRole, ModelStreamEvent, Role, ToolSpec, Turn,
    Usage,
};
use crate::tool::ToolRegistry;

#[derive(Debug)]
pub enum Error {
    Configuration(String),
    Model(model::Error),
    Bus(messagebus::Error),
    MissingDefaultModel,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Configuration(msg) => write!(f, "configuration error: {msg}"),
            Error::Model(err) => write!(f, "model error: {err}"),
            Error::Bus(err) => write!(f, "message bus error: {err}"),
            Error::MissingDefaultModel => write!(f, "missing default model"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Model(err) => Some(err),
            Error::Bus(err) => Some(err),
            _ => None,
        }
    }
}

impl From<model::Error> for Error {
    fn from(err: model::Error) -> Self {
        Error::Model(err)
    }
}

impl From<messagebus::Error> for Error {
    fn from(err: messagebus::Error) -> Self {
        Error::Bus(err)
    }
}

pub struct Agent {
    id: AgentId,
    system_prompt: String,
    models: HashMap<ModelRole, Arc<dyn ARModel>>,
    history: History,
    inbound: BoxStream<'static, Block>,
    bus: Arc<dyn MessageBus>,
    tools: Arc<ToolRegistry>,
}

pub struct AgentBuilder {
    id: Option<AgentId>,
    system_prompt: Option<String>,
    models: HashMap<ModelRole, Arc<dyn ARModel>>,
    tools: Option<Arc<ToolRegistry>>,
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            id: None,
            system_prompt: None,
            models: HashMap::new(),
            tools: None,
        }
    }

    pub fn id(mut self, id: AgentId) -> Self {
        self.id = Some(id);
        self
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn model(mut self, role: ModelRole, model: Arc<dyn ARModel>) -> Self {
        self.models.insert(role, model);
        self
    }

    pub fn tools(mut self, registry: Arc<ToolRegistry>) -> Self {
        self.tools = Some(registry);
        self
    }

    pub fn build(self, bus: Arc<dyn MessageBus>) -> Result<Agent, Error> {
        let id = self
            .id
            .ok_or_else(|| Error::Configuration("agent id is required".to_string()))?;
        let system_prompt = self
            .system_prompt
            .ok_or_else(|| Error::Configuration("system prompt is required".to_string()))?;
        if !self.models.contains_key(&ModelRole::Default) {
            return Err(Error::Configuration(
                "default model is required".to_string(),
            ));
        }
        let inbound = bus.register(id)?;
        let tools = self.tools.unwrap_or_else(|| Arc::new(ToolRegistry::new()));
        Ok(Agent {
            id,
            system_prompt,
            models: self.models,
            history: History::new(),
            inbound,
            bus,
            tools,
        })
    }
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent {
    pub fn id(&self) -> AgentId {
        self.id
    }

    pub fn tools(&self) -> &Arc<ToolRegistry> {
        &self.tools
    }

    pub async fn run(mut self) -> Result<(), Error> {
        while let Some(block) = self.inbound.next().await {
            let inbound_node = self.history.append(block);
            let _ = self.bus.publish(
                self.id,
                AgentEvent::BlockComplete {
                    block: inbound_node,
                },
            );

            loop {
                let context = self.history.linearize();

                let model = self
                    .models
                    .get(&ModelRole::Default)
                    .ok_or(Error::MissingDefaultModel)?
                    .clone();

                let tools: Vec<ToolSpec> = self
                    .tools
                    .list()
                    .into_iter()
                    .map(|tool| ToolSpec {
                        name: tool.name().to_string(),
                        description: tool.description().to_string(),
                        input_schema: tool.input_schema().clone(),
                    })
                    .collect();

                let mut turns: Vec<Turn> = Vec::with_capacity(context.len() + 1);
                turns.push(Turn {
                    role: Role::System,
                    content: vec![ContentPart::Text(self.system_prompt.clone())],
                });
                for block in context {
                    match block {
                        Block::UserMessage { content, .. } => turns.push(Turn {
                            role: Role::User,
                            content: vec![ContentPart::Text(content)],
                        }),
                        Block::AgentMessage { content, .. } => turns.push(Turn {
                            role: Role::Assistant,
                            content: vec![ContentPart::Text(content)],
                        }),
                        Block::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => turns.push(Turn {
                            role: Role::Assistant,
                            content: vec![ContentPart::ToolUse {
                                id,
                                name,
                                input: arguments,
                            }],
                        }),
                        Block::ToolResult {
                            tool_use_id,
                            result,
                            is_error,
                            ..
                        } => turns.push(Turn {
                            role: Role::User,
                            content: vec![ContentPart::ToolResult {
                                tool_use_id,
                                content: result,
                                is_error,
                            }],
                        }),
                        Block::ReasoningTrace { .. } => {
                            // Reasoning traces are kept in History but not routed
                            // to the model; ARMs vary on whether they accept them.
                        }
                    }
                }
                let input = ModelInput { turns, tools };

                let mut stream = model.complete(&input);
                let mut delta_buffer = String::new();
                let mut completed_text: Option<String> = None;
                let mut final_usage = Usage::default();
                let mut pending_tool_calls: Vec<(String, String, serde_json::Value)> = Vec::new();

                while let Some(evt) = stream.next().await {
                    match evt? {
                        ModelStreamEvent::ContentDelta { text } => {
                            delta_buffer.push_str(&text);
                            let _ = self.bus.publish(self.id, AgentEvent::TokenDelta { text });
                        }
                        ModelStreamEvent::ContentComplete { text } => {
                            completed_text = Some(text);
                        }
                        ModelStreamEvent::Usage(u) => {
                            final_usage = u;
                        }
                        ModelStreamEvent::MessageComplete { usage } => {
                            final_usage = usage;
                        }
                        ModelStreamEvent::MessageStart { .. } => {}
                        ModelStreamEvent::Error { .. } => {}
                        ModelStreamEvent::ToolUseStart { .. } => {}
                        ModelStreamEvent::ToolUseDelta { .. } => {}
                        ModelStreamEvent::ToolUseComplete { id, name, input } => {
                            pending_tool_calls.push((id, name, input));
                        }
                    }
                }
                drop(stream);

                let text_content = completed_text.unwrap_or(delta_buffer);
                if !text_content.is_empty() {
                    let agent_node = self.history.append(Block::AgentMessage {
                        from: self.id,
                        content: text_content,
                        timestamp: SystemTime::now(),
                    });
                    let _ = self
                        .bus
                        .publish(self.id, AgentEvent::BlockComplete { block: agent_node });
                }

                let _ = self
                    .bus
                    .publish(self.id, AgentEvent::RunComplete { usage: final_usage });

                if pending_tool_calls.is_empty() {
                    break;
                }

                for (id, name, args) in pending_tool_calls {
                    let call_node = self.history.append(Block::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: args.clone(),
                        timestamp: SystemTime::now(),
                    });
                    let _ = self
                        .bus
                        .publish(self.id, AgentEvent::BlockComplete { block: call_node });

                    let (result_json, is_error) = match self.tools.dispatch(&name, args).await {
                        Ok(value) => (value, false),
                        Err(err) => (serde_json::json!({ "error": err.to_string() }), true),
                    };

                    let result_node = self.history.append(Block::ToolResult {
                        tool_use_id: id,
                        name,
                        result: result_json,
                        is_error,
                        timestamp: SystemTime::now(),
                    });
                    let _ = self
                        .bus
                        .publish(self.id, AgentEvent::BlockComplete { block: result_node });
                }
            }
            let _ = self.bus.publish(self.id, AgentEvent::Idle);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use tokio::time::timeout;

    use crate::history::UserId;
    use crate::messagebus::InMemoryMessageBus;
    use crate::test_support::{EchoTool, StubArModel};

    fn user_block(content: &str) -> Block {
        Block::UserMessage {
            from: UserId::new(),
            content: content.to_string(),
            timestamp: SystemTime::now(),
        }
    }

    fn build_agent(
        model: Arc<dyn ARModel>,
    ) -> (
        Agent,
        AgentId,
        Arc<InMemoryMessageBus>,
        BoxStream<'static, AgentEvent>,
    ) {
        let agent_id = AgentId::new();
        let bus = Arc::new(InMemoryMessageBus::new());
        let agent = AgentBuilder::new()
            .id(agent_id)
            .system_prompt("test prompt")
            .model(ModelRole::Default, model)
            .build(bus.clone() as Arc<dyn MessageBus>)
            .expect("build agent");
        let outbound = bus.subscribe(agent_id).expect("subscribe");
        (agent, agent_id, bus, outbound)
    }

    #[tokio::test]
    async fn agent_run_processes_one_user_message() {
        let model = Arc::new(StubArModel::with_events(vec![
            Ok(ModelStreamEvent::MessageStart {
                message_id: "m1".to_string(),
                model: "stub".to_string(),
            }),
            Ok(ModelStreamEvent::ContentDelta {
                text: "hello".to_string(),
            }),
            Ok(ModelStreamEvent::ContentDelta {
                text: " ".to_string(),
            }),
            Ok(ModelStreamEvent::ContentDelta {
                text: "world".to_string(),
            }),
            Ok(ModelStreamEvent::ContentComplete {
                text: "hello world".to_string(),
            }),
            Ok(ModelStreamEvent::MessageComplete {
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 7,
                },
            }),
        ]));
        let (agent, agent_id, bus, mut outbound) = build_agent(model);

        bus.send_inbound(agent_id, user_block("greet"))
            .expect("send inbound");

        let handle = tokio::spawn(agent.run());

        let mut events = Vec::new();
        let mut idle_seen = false;
        let collect = async {
            while let Some(evt) = outbound.next().await {
                let is_idle = matches!(evt, AgentEvent::Idle);
                events.push(evt);
                if is_idle {
                    idle_seen = true;
                    break;
                }
            }
            events
        };
        let events = timeout(Duration::from_secs(2), collect)
            .await
            .expect("drain timed out");
        assert!(idle_seen, "expected Idle");

        bus.close_inbound(agent_id);
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("join timed out")
            .expect("join")
            .expect("run ok");

        match &events[0] {
            AgentEvent::BlockComplete { block } => match block.block() {
                Block::UserMessage { content, .. } => assert_eq!(content, "greet"),
                other => panic!("expected UserMessage, got {other:?}"),
            },
            other => panic!("expected BlockComplete, got {other:?}"),
        }

        let deltas: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TokenDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["hello", " ", "world"]);

        let block_completes: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::BlockComplete { block } => Some(block.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(block_completes.len(), 2);
        match block_completes[1].block() {
            Block::AgentMessage { content, .. } => assert_eq!(content, "hello world"),
            other => panic!("expected AgentMessage, got {other:?}"),
        }

        match events.last().expect("at least one event") {
            AgentEvent::Idle => {}
            other => panic!("expected Idle, got {other:?}"),
        }
        let penultimate_idx = events.len().checked_sub(2).expect("at least two events");
        match &events[penultimate_idx] {
            AgentEvent::RunComplete { usage } => {
                assert_eq!(usage.input_tokens, 5);
                assert_eq!(usage.output_tokens, 7);
            }
            other => panic!("expected RunComplete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_run_exits_when_inbound_closed() {
        let model = Arc::new(StubArModel::empty());
        let (agent, agent_id, bus, _outbound) = build_agent(model);

        let handle = tokio::spawn(agent.run());
        bus.close_inbound(agent_id);
        let result = timeout(Duration::from_secs(1), handle)
            .await
            .expect("join timed out")
            .expect("join");
        result.expect("clean exit");
    }

    #[tokio::test]
    async fn agent_run_propagates_model_error() {
        let model = Arc::new(StubArModel::with_events(vec![Err(model::Error::Api {
            status: 500,
            message: "boom".to_string(),
        })]));
        let (agent, agent_id, bus, _outbound) = build_agent(model);

        bus.send_inbound(agent_id, user_block("trigger"))
            .expect("send inbound");

        let handle = tokio::spawn(agent.run());
        let result = timeout(Duration::from_secs(1), handle)
            .await
            .expect("join timed out")
            .expect("join");
        match result {
            Err(Error::Model(model::Error::Api { status, message })) => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Error::Model(Api), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_run_uses_delta_buffer_when_no_content_complete() {
        let model = Arc::new(StubArModel::with_events(vec![
            Ok(ModelStreamEvent::ContentDelta {
                text: "fragment".to_string(),
            }),
            Ok(ModelStreamEvent::MessageComplete {
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            }),
        ]));
        let (agent, agent_id, bus, mut outbound) = build_agent(model);

        bus.send_inbound(agent_id, user_block("prompt"))
            .expect("send inbound");

        let handle = tokio::spawn(agent.run());

        let mut events = Vec::new();
        let collect = async {
            while let Some(evt) = outbound.next().await {
                let stop = matches!(evt, AgentEvent::Idle);
                events.push(evt);
                if stop {
                    break;
                }
            }
            events
        };
        let events = timeout(Duration::from_secs(2), collect)
            .await
            .expect("drain timed out");

        bus.close_inbound(agent_id);
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("join timed out")
            .expect("join")
            .expect("run ok");

        let agent_block = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::BlockComplete { block } => match block.block() {
                    Block::AgentMessage { content, .. } => Some(content.clone()),
                    _ => None,
                },
                _ => None,
            })
            .next()
            .expect("at least one AgentMessage block");
        assert_eq!(agent_block, "fragment");
    }

    #[tokio::test]
    async fn agent_run_compiles_history_into_model_input_with_system_turn() {
        let model = Arc::new(StubArModel::with_events(vec![
            Ok(ModelStreamEvent::ContentComplete {
                text: "ok".to_string(),
            }),
            Ok(ModelStreamEvent::MessageComplete {
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            }),
        ]));
        let captured = model.clone();
        let (agent, agent_id, bus, mut outbound) = build_agent(model);

        bus.send_inbound(agent_id, user_block("hello there"))
            .expect("send inbound");

        let handle = tokio::spawn(agent.run());

        let drain = async {
            while let Some(evt) = outbound.next().await {
                if matches!(evt, AgentEvent::Idle) {
                    break;
                }
            }
        };
        timeout(Duration::from_secs(2), drain)
            .await
            .expect("drain timed out");

        bus.close_inbound(agent_id);
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("join timed out")
            .expect("join")
            .expect("run ok");

        let input = captured.last_input().expect("model called");
        assert_eq!(input.turns.len(), 2);
        assert_eq!(input.turns[0].role, Role::System);
        match &input.turns[0].content[0] {
            ContentPart::Text(t) => assert_eq!(t, "test prompt"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert_eq!(input.turns[1].role, Role::User);
        match &input.turns[1].content[0] {
            ContentPart::Text(t) => assert_eq!(t, "hello there"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn agent_default_tool_registry_is_empty() {
        let model = Arc::new(StubArModel::empty());
        let (agent, _agent_id, _bus, _outbound) = build_agent(model);
        assert!(agent.tools().list().is_empty());
    }

    #[test]
    fn agent_tools_accessor_returns_shared_registry() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(EchoTool::new("echo")))
            .expect("register echo");
        let registry = Arc::new(registry);

        let agent_id = AgentId::new();
        let bus = Arc::new(InMemoryMessageBus::new());
        let model = Arc::new(StubArModel::empty());
        let agent = AgentBuilder::new()
            .id(agent_id)
            .system_prompt("test prompt")
            .model(ModelRole::Default, model as Arc<dyn ARModel>)
            .tools(registry)
            .build(bus.clone() as Arc<dyn MessageBus>)
            .expect("build agent");
        assert!(agent.tools().get("echo").is_some());
    }

    #[tokio::test]
    async fn agent_run_populates_model_input_tools_from_registry() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(EchoTool::new("echo")))
            .expect("register echo");
        let registry = Arc::new(registry);

        let model = Arc::new(StubArModel::with_events(vec![
            Ok(ModelStreamEvent::ContentComplete {
                text: "ok".to_string(),
            }),
            Ok(ModelStreamEvent::MessageComplete {
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            }),
        ]));
        let captured = model.clone();

        let agent_id = AgentId::new();
        let bus = Arc::new(InMemoryMessageBus::new());
        let agent = AgentBuilder::new()
            .id(agent_id)
            .system_prompt("test prompt")
            .model(ModelRole::Default, model as Arc<dyn ARModel>)
            .tools(registry)
            .build(bus.clone() as Arc<dyn MessageBus>)
            .expect("build agent");
        let mut outbound = bus.subscribe(agent_id).expect("subscribe");

        bus.send_inbound(agent_id, user_block("hello"))
            .expect("send inbound");

        let handle = tokio::spawn(agent.run());

        let drain = async {
            while let Some(evt) = outbound.next().await {
                if matches!(evt, AgentEvent::Idle) {
                    break;
                }
            }
        };
        timeout(Duration::from_secs(2), drain)
            .await
            .expect("drain timed out");

        bus.close_inbound(agent_id);
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("join timed out")
            .expect("join")
            .expect("run ok");

        let input = captured.last_input().expect("model called");
        assert_eq!(input.tools.len(), 1);
        assert_eq!(input.tools[0].name, "echo");
        assert_eq!(
            input.tools[0].description,
            "Echoes input back wrapped under `echo`."
        );
    }

    #[tokio::test]
    async fn agent_run_compiles_block_tool_call_into_content_part_tool_use() {
        let model = Arc::new(StubArModel::with_events(vec![
            Ok(ModelStreamEvent::ContentComplete {
                text: "ok".to_string(),
            }),
            Ok(ModelStreamEvent::MessageComplete {
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            }),
        ]));
        let captured = model.clone();
        let (agent, agent_id, bus, mut outbound) = build_agent(model);

        bus.send_inbound(
            agent_id,
            Block::ToolCall {
                id: "use_x".to_string(),
                name: "echo".to_string(),
                arguments: serde_json::json!({ "a": 1 }),
                timestamp: SystemTime::now(),
            },
        )
        .expect("send tool call");
        bus.send_inbound(agent_id, user_block("follow up"))
            .expect("send user message");

        let handle = tokio::spawn(agent.run());

        let drain = async {
            let mut idles = 0;
            while let Some(evt) = outbound.next().await {
                if matches!(evt, AgentEvent::Idle) {
                    idles += 1;
                    if idles == 2 {
                        break;
                    }
                }
            }
        };
        timeout(Duration::from_secs(2), drain)
            .await
            .expect("drain timed out");

        bus.close_inbound(agent_id);
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("join timed out")
            .expect("join")
            .expect("run ok");

        let input = captured.last_input().expect("model called");
        let tool_use_turn = input
            .turns
            .iter()
            .find(|t| {
                t.role == Role::Assistant
                    && matches!(t.content.first(), Some(ContentPart::ToolUse { .. }))
            })
            .expect("expected an Assistant turn with ContentPart::ToolUse");
        match &tool_use_turn.content[0] {
            ContentPart::ToolUse { id, name, input } => {
                assert_eq!(id, "use_x");
                assert_eq!(name, "echo");
                assert_eq!(input, &serde_json::json!({ "a": 1 }));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_run_compiles_block_tool_result_into_content_part_tool_result() {
        let model = Arc::new(StubArModel::with_events(vec![
            Ok(ModelStreamEvent::ContentComplete {
                text: "ok".to_string(),
            }),
            Ok(ModelStreamEvent::MessageComplete {
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            }),
        ]));
        let captured = model.clone();
        let (agent, agent_id, bus, mut outbound) = build_agent(model);

        bus.send_inbound(
            agent_id,
            Block::ToolResult {
                tool_use_id: "use_x".to_string(),
                name: "echo".to_string(),
                result: serde_json::json!("ok"),
                is_error: false,
                timestamp: SystemTime::now(),
            },
        )
        .expect("send tool result");
        bus.send_inbound(agent_id, user_block("follow up"))
            .expect("send user message");

        let handle = tokio::spawn(agent.run());

        let drain = async {
            let mut idles = 0;
            while let Some(evt) = outbound.next().await {
                if matches!(evt, AgentEvent::Idle) {
                    idles += 1;
                    if idles == 2 {
                        break;
                    }
                }
            }
        };
        timeout(Duration::from_secs(2), drain)
            .await
            .expect("drain timed out");

        bus.close_inbound(agent_id);
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("join timed out")
            .expect("join")
            .expect("run ok");

        let input = captured.last_input().expect("model called");
        let tool_result_turn = input
            .turns
            .iter()
            .find(|t| {
                t.role == Role::User
                    && matches!(t.content.first(), Some(ContentPart::ToolResult { .. }))
            })
            .expect("expected a User turn with ContentPart::ToolResult");
        match &tool_result_turn.content[0] {
            ContentPart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "use_x");
                assert_eq!(content, &serde_json::json!("ok"));
                assert!(!*is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    async fn drain_until_idle(
        outbound: &mut BoxStream<'static, AgentEvent>,
        sink: &mut Vec<AgentEvent>,
    ) {
        while let Some(evt) = outbound.next().await {
            let is_idle = matches!(evt, AgentEvent::Idle);
            sink.push(evt);
            if is_idle {
                break;
            }
        }
    }

    #[tokio::test]
    async fn agent_run_dispatches_tool_call_and_re_enters_model() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(EchoTool::new("echo")))
            .expect("register echo");
        let registry = Arc::new(registry);

        let model = Arc::new(StubArModel::with_call_scripts(vec![
            vec![
                Ok(ModelStreamEvent::MessageStart {
                    message_id: "m1".to_string(),
                    model: "stub".to_string(),
                }),
                Ok(ModelStreamEvent::ToolUseStart {
                    id: "use_1".to_string(),
                    name: "echo".to_string(),
                }),
                Ok(ModelStreamEvent::ToolUseComplete {
                    id: "use_1".to_string(),
                    name: "echo".to_string(),
                    input: serde_json::json!({ "text": "hi" }),
                }),
                Ok(ModelStreamEvent::MessageComplete {
                    usage: Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                    },
                }),
            ],
            vec![
                Ok(ModelStreamEvent::MessageStart {
                    message_id: "m2".to_string(),
                    model: "stub".to_string(),
                }),
                Ok(ModelStreamEvent::ContentDelta {
                    text: "done".to_string(),
                }),
                Ok(ModelStreamEvent::ContentComplete {
                    text: "done".to_string(),
                }),
                Ok(ModelStreamEvent::MessageComplete {
                    usage: Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                }),
            ],
        ]));
        let captured = model.clone();

        let agent_id = AgentId::new();
        let bus = Arc::new(InMemoryMessageBus::new());
        let agent = AgentBuilder::new()
            .id(agent_id)
            .system_prompt("test prompt")
            .model(ModelRole::Default, model as Arc<dyn ARModel>)
            .tools(registry)
            .build(bus.clone() as Arc<dyn MessageBus>)
            .expect("build agent");
        let mut outbound = bus.subscribe(agent_id).expect("subscribe");

        bus.send_inbound(agent_id, user_block("please call echo"))
            .expect("send inbound");

        let handle = tokio::spawn(agent.run());

        let mut events: Vec<AgentEvent> = Vec::new();
        timeout(
            Duration::from_secs(2),
            drain_until_idle(&mut outbound, &mut events),
        )
        .await
        .expect("drain timed out");

        bus.close_inbound(agent_id);
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("join timed out")
            .expect("join")
            .expect("run ok");

        let input = captured.last_input().expect("model called");
        let tool_use_turn = input
            .turns
            .iter()
            .find(|t| {
                t.role == Role::Assistant
                    && matches!(t.content.first(), Some(ContentPart::ToolUse { .. }))
            })
            .expect("expected Assistant turn with ToolUse");
        match &tool_use_turn.content[0] {
            ContentPart::ToolUse { id, name, input } => {
                assert_eq!(id, "use_1");
                assert_eq!(name, "echo");
                assert_eq!(input, &serde_json::json!({ "text": "hi" }));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }

        let tool_result_turn = input
            .turns
            .iter()
            .find(|t| {
                t.role == Role::User
                    && matches!(t.content.first(), Some(ContentPart::ToolResult { .. }))
            })
            .expect("expected User turn with ToolResult");
        match &tool_result_turn.content[0] {
            ContentPart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "use_1");
                assert_eq!(content, &serde_json::json!({ "echo": { "text": "hi" } }));
                assert!(!*is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }

        let block_complete_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::BlockComplete { .. }))
            .count();
        assert_eq!(block_complete_count, 4);

        let run_complete_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::RunComplete { .. }))
            .count();
        assert_eq!(run_complete_count, 2);

        let agent_messages: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::BlockComplete { block } => match block.block() {
                    Block::AgentMessage { content, .. } => Some(content.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(agent_messages, vec!["done".to_string()]);
    }

    #[tokio::test]
    async fn agent_run_dispatches_unknown_tool_yields_error_result() {
        let model = Arc::new(StubArModel::with_call_scripts(vec![
            vec![
                Ok(ModelStreamEvent::ToolUseComplete {
                    id: "use_x".to_string(),
                    name: "nonexistent".to_string(),
                    input: serde_json::Value::Null,
                }),
                Ok(ModelStreamEvent::MessageComplete {
                    usage: Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                    },
                }),
            ],
            vec![
                Ok(ModelStreamEvent::ContentComplete {
                    text: "recovered".to_string(),
                }),
                Ok(ModelStreamEvent::MessageComplete {
                    usage: Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                }),
            ],
        ]));
        let captured = model.clone();
        let (agent, agent_id, bus, mut outbound) = build_agent(model);

        bus.send_inbound(agent_id, user_block("trigger missing tool"))
            .expect("send inbound");

        let handle = tokio::spawn(agent.run());

        let mut events: Vec<AgentEvent> = Vec::new();
        timeout(
            Duration::from_secs(2),
            drain_until_idle(&mut outbound, &mut events),
        )
        .await
        .expect("drain timed out");

        bus.close_inbound(agent_id);
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("join timed out")
            .expect("join")
            .expect("run ok");

        let input = captured.last_input().expect("model called");
        let tool_result_turn = input
            .turns
            .iter()
            .find(|t| {
                t.role == Role::User
                    && matches!(t.content.first(), Some(ContentPart::ToolResult { .. }))
            })
            .expect("expected User turn with ToolResult");
        match &tool_result_turn.content[0] {
            ContentPart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "use_x");
                assert!(*is_error);
                let serialized = serde_json::to_string(content).expect("serialize content");
                assert!(
                    serialized.contains("nonexistent"),
                    "expected error to mention tool name, got {serialized}"
                );
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_run_skips_agent_message_when_only_tool_calls() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(EchoTool::new("echo")))
            .expect("register echo");
        let registry = Arc::new(registry);

        let model = Arc::new(StubArModel::with_call_scripts(vec![
            vec![
                Ok(ModelStreamEvent::ToolUseComplete {
                    id: "use_1".to_string(),
                    name: "echo".to_string(),
                    input: serde_json::json!({ "text": "hi" }),
                }),
                Ok(ModelStreamEvent::MessageComplete {
                    usage: Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                    },
                }),
            ],
            vec![
                Ok(ModelStreamEvent::ContentComplete {
                    text: "final".to_string(),
                }),
                Ok(ModelStreamEvent::MessageComplete {
                    usage: Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                }),
            ],
        ]));
        let captured = model.clone();

        let agent_id = AgentId::new();
        let bus = Arc::new(InMemoryMessageBus::new());
        let agent = AgentBuilder::new()
            .id(agent_id)
            .system_prompt("test prompt")
            .model(ModelRole::Default, model as Arc<dyn ARModel>)
            .tools(registry)
            .build(bus.clone() as Arc<dyn MessageBus>)
            .expect("build agent");
        let mut outbound = bus.subscribe(agent_id).expect("subscribe");

        bus.send_inbound(agent_id, user_block("call echo only"))
            .expect("send inbound");

        let handle = tokio::spawn(agent.run());

        let mut events: Vec<AgentEvent> = Vec::new();
        timeout(
            Duration::from_secs(2),
            drain_until_idle(&mut outbound, &mut events),
        )
        .await
        .expect("drain timed out");

        bus.close_inbound(agent_id);
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("join timed out")
            .expect("join")
            .expect("run ok");

        let input = captured.last_input().expect("model called");
        let has_empty_assistant_text = input.turns.iter().any(|t| {
            t.role == Role::Assistant
                && t.content.len() == 1
                && matches!(&t.content[0], ContentPart::Text(s) if s.is_empty())
        });
        assert!(
            !has_empty_assistant_text,
            "did not expect an Assistant turn with empty Text; turns: {:?}",
            input.turns
        );
    }
}
