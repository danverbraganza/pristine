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
    self, ARModel, ContentPart, ModelInput, ModelRole, ModelStreamEvent, Role, Turn, Usage,
};

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
}

pub struct AgentBuilder {
    id: Option<AgentId>,
    system_prompt: Option<String>,
    models: HashMap<ModelRole, Arc<dyn ARModel>>,
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            id: None,
            system_prompt: None,
            models: HashMap::new(),
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
        Ok(Agent {
            id,
            system_prompt,
            models: self.models,
            history: History::new(),
            inbound,
            bus,
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

    pub async fn run(mut self) -> Result<(), Error> {
        while let Some(block) = self.inbound.next().await {
            let inbound_node = self.history.append(block);
            let _ = self.bus.publish(
                self.id,
                AgentEvent::BlockComplete {
                    block: inbound_node,
                },
            );

            let context = self.history.linearize();

            let model = self
                .models
                .get(&ModelRole::Default)
                .ok_or(Error::MissingDefaultModel)?
                .clone();

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
                    Block::ReasoningTrace { .. }
                    | Block::ToolCall { .. }
                    | Block::ToolResult { .. } => {
                        // Phase 1: these variants are kept in History but not
                        // routed to the model. Phase 2 will route them via
                        // ContentPart::ToolUse / ContentPart::ToolResult.
                    }
                }
            }
            let input = ModelInput { turns };

            let mut stream = model.complete(&input);
            let mut delta_buffer = String::new();
            let mut completed_text: Option<String> = None;
            let mut final_usage = Usage::default();

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
                }
            }
            drop(stream);

            let content = completed_text.unwrap_or(delta_buffer);
            let agent_msg = Block::AgentMessage {
                from: self.id,
                content,
                timestamp: SystemTime::now(),
            };
            let agent_node = self.history.append(agent_msg);
            let _ = self
                .bus
                .publish(self.id, AgentEvent::BlockComplete { block: agent_node });
            let _ = self
                .bus
                .publish(self.id, AgentEvent::RunComplete { usage: final_usage });
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
    use crate::test_support::StubArModel;

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
        let mut run_complete_seen = false;
        let collect = async {
            while let Some(evt) = outbound.next().await {
                let is_run_complete = matches!(evt, AgentEvent::RunComplete { .. });
                events.push(evt);
                if is_run_complete {
                    run_complete_seen = true;
                    break;
                }
            }
            events
        };
        let events = timeout(Duration::from_secs(2), collect)
            .await
            .expect("drain timed out");
        assert!(run_complete_seen, "expected RunComplete");

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
                let stop = matches!(evt, AgentEvent::RunComplete { .. });
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
                if matches!(evt, AgentEvent::RunComplete { .. }) {
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
}
