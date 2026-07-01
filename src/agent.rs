//! Agent and event loop.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use futures::StreamExt;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::builtins::fork::fork_from_context;
use crate::harness::{AgentSpawner, DisconnectedSpawner};
pub use crate::history::AgentId;
use crate::history::{Block, CheckpointHandle, History, HistoryNode};
use crate::messagebus::{self, AgentEvent, Control, MessageBus};
use crate::model::{
    self, ARModel, ContentPart, ModelInput, ModelRole, ModelStreamEvent, Role, ToolSpec, Turn,
    Usage,
};
use crate::skills::SkillsRegistrySource;
use crate::tool::{ToolCallContext, ToolError, ToolRegistry};

/// Prefix marking the harness-attributed checkpoint-handle line appended to a
/// tool result when History is compiled into a [`ModelInput`].
///
/// The line is injected at compile time only; the stored `Block::ToolResult`
/// is never mutated. Naming this checkpoint lets an agent address the
/// tool-call boundary when forking.
const CHECKPOINT_ANNOTATION_PREFIX: &str = "[pristine checkpoint]";

/// Render a tool result's stored value into the model-visible content, with a
/// harness-attributed checkpoint-handle line appended.
///
/// The base rendering matches the provider adapters' convention (a string is
/// used verbatim; any other JSON value is stringified), so the only change the
/// model sees is the trailing handle line.
fn annotate_tool_result_content(
    result: serde_json::Value,
    handle: CheckpointHandle,
) -> serde_json::Value {
    let base = match result {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    };
    serde_json::Value::String(format!("{base}\n\n{CHECKPOINT_ANNOTATION_PREFIX} {handle}"))
}

/// Structured system prompt with named-field slots.
///
/// Replaces the agent's former `system_prompt: String`. The fixed `base` text
/// is authored in config; the dynamic `skills` slot, when present, contributes
/// a tier-1 skills disclosure section. [`render`](SystemPrompt::render) is
/// called once per agent iteration so catalog growth between turns is picked
/// up without rebuilding the agent.
#[derive(Clone)]
pub struct SystemPrompt {
    pub base: String,
    pub skills: Option<Arc<dyn SkillsRegistrySource>>,
}

impl SystemPrompt {
    /// Render the prompt into the text placed in the system `Turn`.
    ///
    /// Returns `self.base` unchanged when there is no skills slot or the slot's
    /// catalog is empty. When the slot is populated, appends a tier-1
    /// `## Available skills` markdown section after the base: one bullet per
    /// skill (`**name**: description`) and a closing line pointing the model at
    /// the `activate_skill` tool.
    pub fn render(&self) -> String {
        let Some(skills) = &self.skills else {
            return self.base.clone();
        };
        let summaries = skills.list();
        if summaries.is_empty() {
            return self.base.clone();
        }

        let mut out = self.base.clone();
        out.push_str("\n\n## Available skills\n\n");
        for summary in &summaries {
            out.push_str(&format!(
                "- **{}**: {}\n",
                summary.name, summary.description
            ));
        }
        out.push_str("\nTo use a skill, call the `activate_skill` tool with the skill's name.");
        out
    }
}

impl std::fmt::Debug for SystemPrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemPrompt")
            .field("base", &self.base)
            .field("skills", &self.skills.as_ref().map(|_| "<source>"))
            .finish()
    }
}

/// A unified inbound item for the Agent's run loop, merging the `Block` inbound
/// stream with the out-of-band control stream. `Block`s flow through the normal
/// model cycle; `Control` messages drive the Agent without a model turn.
enum Inbound {
    Block(Block),
    Control(Control),
}

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
    system_prompt: SystemPrompt,
    models: HashMap<ModelRole, Arc<dyn ARModel>>,
    history: History,
    inbound: BoxStream<'static, Inbound>,
    bus: Arc<dyn MessageBus>,
    tools: Arc<ToolRegistry>,
    spawner: Arc<dyn AgentSpawner>,
    stop_token: CancellationToken,
}

pub struct AgentBuilder {
    id: Option<AgentId>,
    system_prompt: Option<SystemPrompt>,
    models: HashMap<ModelRole, Arc<dyn ARModel>>,
    tools: Option<Arc<ToolRegistry>>,
    history_prefix: Option<Arc<HistoryNode>>,
    spawner: Option<Arc<dyn AgentSpawner>>,
    stop_token: Option<CancellationToken>,
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            id: None,
            system_prompt: None,
            models: HashMap::new(),
            tools: None,
            history_prefix: None,
            spawner: None,
            stop_token: None,
        }
    }

    pub fn id(mut self, id: AgentId) -> Self {
        self.id = Some(id);
        self
    }

    pub fn system_prompt(mut self, prompt: SystemPrompt) -> Self {
        self.system_prompt = Some(prompt);
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

    /// Seed the built Agent's `History` from an inherited prefix head.
    ///
    /// `Some(head)` starts the log at the given node, sharing the parent chain
    /// via `Arc`; `None` (the default) yields an empty log. This is the seam a
    /// runtime fork uses to hand a new peer Agent a bounded slice of the
    /// initiator's context.
    pub fn history_prefix(mut self, head: Option<Arc<HistoryNode>>) -> Self {
        self.history_prefix = head;
        self
    }

    /// Attach the runtime spawner the built Agent hands to agent-aware tools via
    /// [`ToolCallContext`]. Defaults to a disconnected spawner that errors on
    /// use when unset (e.g. Agents built outside a running Harness).
    pub fn spawner(mut self, spawner: Arc<dyn AgentSpawner>) -> Self {
        self.spawner = Some(spawner);
        self
    }

    /// Attach the built Agent's per-agent cancellation child token, exposed to
    /// tools as the self-stop handle and observed by the run loop. Defaults to a
    /// detached token when unset, so an unstopped standalone Agent's self-stop
    /// is a harmless no-op.
    pub fn stop_token(mut self, token: CancellationToken) -> Self {
        self.stop_token = Some(token);
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
        let blocks = bus.register(id)?;
        let control = bus.control_stream(id)?;
        // Merge the Block inbound and control streams into one so the run loop
        // serializes over both; either stream closing narrows the merge, and
        // both closing ends it.
        let inbound =
            futures::stream::select(blocks.map(Inbound::Block), control.map(Inbound::Control))
                .boxed();
        let tools = self.tools.unwrap_or_else(|| Arc::new(ToolRegistry::new()));
        let spawner = self
            .spawner
            .unwrap_or_else(|| Arc::new(DisconnectedSpawner) as Arc<dyn AgentSpawner>);
        let stop_token = self.stop_token.unwrap_or_default();
        Ok(Agent {
            id,
            system_prompt,
            models: self.models,
            history: History::from_prefix(self.history_prefix),
            inbound,
            bus,
            tools,
            spawner,
            stop_token,
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

    /// Handle an out-of-band control message. Control messages drive the Agent
    /// without appending a `Block` or triggering a model turn for the request
    /// itself. A fork request is resolved against the same runtime context the
    /// Agent assembles at tool dispatch and delegated to the shared fork logic;
    /// a malformed/invalid request surfaces as an error event and spawns nothing.
    fn handle_control(&self, control: Control) {
        match control {
            Control::Fork(request) => {
                let ctx = ToolCallContext::new(
                    self.id,
                    self.history.head().cloned(),
                    self.system_prompt.clone(),
                    self.models.clone(),
                    self.tools.clone(),
                    self.spawner.clone(),
                    self.stop_token.clone(),
                );
                if let Err(err) =
                    fork_from_context(&ctx, request.instruction, request.handle, request.tools)
                {
                    let _ = self.bus.publish(
                        self.id,
                        AgentEvent::Error {
                            message: err.to_string(),
                        },
                    );
                }
            }
        }
    }

    pub async fn run(mut self) -> Result<(), Error> {
        while let Some(inbound) = self.inbound.next().await {
            let block = match inbound {
                Inbound::Control(control) => {
                    self.handle_control(control);
                    continue;
                }
                Inbound::Block(block) => block,
            };
            let inbound_node = self.history.append(block);
            let _ = self.bus.publish(
                self.id,
                AgentEvent::BlockComplete {
                    block: inbound_node,
                },
            );

            loop {
                let context = self.history.linearize_with_handles();

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
                    content: vec![ContentPart::Text(self.system_prompt.render())],
                });
                for (handle, block) in context {
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
                                content: annotate_tool_result_content(result, handle),
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
                let mut reasoning_delta_buffer = String::new();
                let mut reasoning_completed: Option<String> = None;
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
                        ModelStreamEvent::ReasoningDelta { text } => {
                            reasoning_delta_buffer.push_str(&text);
                            let _ = self
                                .bus
                                .publish(self.id, AgentEvent::ReasoningDelta { text });
                        }
                        ModelStreamEvent::ReasoningComplete { text } => {
                            reasoning_completed = Some(text);
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

                let reasoning_content = reasoning_completed.unwrap_or(reasoning_delta_buffer);
                if !reasoning_content.is_empty() {
                    let reasoning_node = self.history.append(Block::ReasoningTrace {
                        content: reasoning_content,
                        timestamp: SystemTime::now(),
                    });
                    let _ = self.bus.publish(
                        self.id,
                        AgentEvent::BlockComplete {
                            block: reasoning_node,
                        },
                    );
                }

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

                    let ctx = ToolCallContext::new(
                        self.id,
                        self.history.head().cloned(),
                        self.system_prompt.clone(),
                        self.models.clone(),
                        self.tools.clone(),
                        self.spawner.clone(),
                        self.stop_token.clone(),
                    );
                    let (result_json, is_error) =
                        match self.tools.dispatch_with_context(&name, args, &ctx).await {
                            Ok(value) => (value, false),
                            Err(err) => {
                                let value = match err {
                                    ToolError::Execution(v) => v,
                                    other => serde_json::json!({ "error": other.to_string() }),
                                };
                                (value, true)
                            }
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
    use crate::skills::SkillRecord;
    use crate::test_support::{EchoTool, StubArModel, StubSkillsRegistry};

    fn skill_record(name: &str, description: &str) -> SkillRecord {
        SkillRecord {
            name: name.to_string(),
            description: description.to_string(),
            body: String::new(),
            directory: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn system_prompt_render_returns_base_when_no_skills_slot() {
        let prompt = SystemPrompt {
            base: "you are pristine".to_string(),
            skills: None,
        };
        assert_eq!(prompt.render(), "you are pristine");
    }

    #[test]
    fn system_prompt_render_returns_base_when_skills_slot_is_empty() {
        let prompt = SystemPrompt {
            base: "you are pristine".to_string(),
            skills: Some(Arc::new(StubSkillsRegistry::new(Vec::new()))),
        };
        assert_eq!(prompt.render(), "you are pristine");
    }

    #[test]
    fn system_prompt_render_appends_skills_section_when_slot_populated() {
        let prompt = SystemPrompt {
            base: "you are pristine".to_string(),
            skills: Some(Arc::new(StubSkillsRegistry::new(vec![
                skill_record("demo", "a demo skill"),
                skill_record("other", "another skill"),
            ]))),
        };
        let expected = "you are pristine\n\n## Available skills\n\n\
- **demo**: a demo skill\n\
- **other**: another skill\n\
\nTo use a skill, call the `activate_skill` tool with the skill's name.";
        assert_eq!(prompt.render(), expected);
    }

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
            .system_prompt(SystemPrompt {
                base: "test prompt".to_string(),
                skills: None,
            })
            .model(ModelRole::Default, model)
            .build(bus.clone() as Arc<dyn MessageBus>)
            .expect("build agent");
        let outbound = bus.subscribe(agent_id).expect("subscribe");
        (agent, agent_id, bus, outbound)
    }

    #[tokio::test]
    async fn agent_run_processes_one_user_message() -> Result<(), Box<dyn std::error::Error>> {
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

        let mut events: Vec<AgentEvent> = Vec::new();
        timeout(
            Duration::from_secs(2),
            drain_until_idles(&mut outbound, &mut events, 1),
        )
        .await
        .expect("drain timed out");

        bus.close_inbound(agent_id);
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("join timed out")
            .expect("join")
            .expect("run ok");

        match &events[0] {
            AgentEvent::BlockComplete { block } => match block.block() {
                Block::UserMessage { content, .. } => assert_eq!(content, "greet"),
                other => return Err(format!("expected UserMessage, got {other:?}").into()),
            },
            other => return Err(format!("expected BlockComplete, got {other:?}").into()),
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
            other => return Err(format!("expected AgentMessage, got {other:?}").into()),
        }

        match events.last().expect("at least one event") {
            AgentEvent::Idle => {}
            other => return Err(format!("expected Idle, got {other:?}").into()),
        }
        let penultimate_idx = events.len().checked_sub(2).expect("at least two events");
        match &events[penultimate_idx] {
            AgentEvent::RunComplete { usage } => {
                assert_eq!(usage.input_tokens, 5);
                assert_eq!(usage.output_tokens, 7);
            }
            other => return Err(format!("expected RunComplete, got {other:?}").into()),
        }
        Ok(())
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
    async fn agent_run_propagates_model_error() -> Result<(), Box<dyn std::error::Error>> {
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
            other => return Err(format!("expected Error::Model(Api), got {other:?}").into()),
        }
        Ok(())
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

        let mut events: Vec<AgentEvent> = Vec::new();
        timeout(
            Duration::from_secs(2),
            drain_until_idles(&mut outbound, &mut events, 1),
        )
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
    async fn agent_run_compiles_history_into_model_input_with_system_turn()
    -> Result<(), Box<dyn std::error::Error>> {
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

        let mut events: Vec<AgentEvent> = Vec::new();
        timeout(
            Duration::from_secs(2),
            drain_until_idles(&mut outbound, &mut events, 1),
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
        assert_eq!(input.turns.len(), 2);
        assert_eq!(input.turns[0].role, Role::System);
        match &input.turns[0].content[0] {
            ContentPart::Text(t) => assert_eq!(t, "test prompt"),
            other => return Err(format!("expected Text, got {other:?}").into()),
        }
        assert_eq!(input.turns[1].role, Role::User);
        match &input.turns[1].content[0] {
            ContentPart::Text(t) => assert_eq!(t, "hello there"),
            other => return Err(format!("expected Text, got {other:?}").into()),
        }
        Ok(())
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
            .system_prompt(SystemPrompt {
                base: "test prompt".to_string(),
                skills: None,
            })
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
            .system_prompt(SystemPrompt {
                base: "test prompt".to_string(),
                skills: None,
            })
            .model(ModelRole::Default, model as Arc<dyn ARModel>)
            .tools(registry)
            .build(bus.clone() as Arc<dyn MessageBus>)
            .expect("build agent");
        let mut outbound = bus.subscribe(agent_id).expect("subscribe");

        bus.send_inbound(agent_id, user_block("hello"))
            .expect("send inbound");

        let handle = tokio::spawn(agent.run());

        let mut events: Vec<AgentEvent> = Vec::new();
        timeout(
            Duration::from_secs(2),
            drain_until_idles(&mut outbound, &mut events, 1),
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
        assert_eq!(input.tools.len(), 1);
        assert_eq!(input.tools[0].name, "echo");
        assert_eq!(
            input.tools[0].description,
            "Echoes input back wrapped under `echo`."
        );
    }

    #[tokio::test]
    async fn agent_run_compiles_block_tool_call_into_content_part_tool_use()
    -> Result<(), Box<dyn std::error::Error>> {
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

        let mut events: Vec<AgentEvent> = Vec::new();
        timeout(
            Duration::from_secs(2),
            drain_until_idles(&mut outbound, &mut events, 2),
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
            .expect("expected an Assistant turn with ContentPart::ToolUse");
        match &tool_use_turn.content[0] {
            ContentPart::ToolUse { id, name, input } => {
                assert_eq!(id, "use_x");
                assert_eq!(name, "echo");
                assert_eq!(input, &serde_json::json!({ "a": 1 }));
            }
            other => return Err(format!("expected ToolUse, got {other:?}").into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn agent_run_compiles_block_tool_result_into_content_part_tool_result()
    -> Result<(), Box<dyn std::error::Error>> {
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

        let mut events: Vec<AgentEvent> = Vec::new();
        timeout(
            Duration::from_secs(2),
            drain_until_idles(&mut outbound, &mut events, 2),
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
            .expect("expected a User turn with ContentPart::ToolResult");
        match &tool_result_turn.content[0] {
            ContentPart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "use_x");
                let rendered = content
                    .as_str()
                    .ok_or("expected tool result content to be a string")?;
                assert!(rendered.starts_with("ok"));
                assert!(rendered.contains("[pristine checkpoint] ckpt-"));
                assert!(!*is_error);
            }
            other => return Err(format!("expected ToolResult, got {other:?}").into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn agent_run_injects_checkpoint_handle_into_tool_result_and_preserves_block()
    -> Result<(), Box<dyn std::error::Error>> {
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
                result: serde_json::json!("raw output"),
                is_error: false,
                timestamp: SystemTime::now(),
            },
        )
        .expect("send tool result");
        bus.send_inbound(agent_id, user_block("follow up"))
            .expect("send user message");

        let handle = tokio::spawn(agent.run());

        let mut events: Vec<AgentEvent> = Vec::new();
        timeout(
            Duration::from_secs(2),
            drain_until_idles(&mut outbound, &mut events, 2),
        )
        .await
        .expect("drain timed out");

        bus.close_inbound(agent_id);
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("join timed out")
            .expect("join")
            .expect("run ok");

        // The tool-result node carries the handle we expect the compiled input
        // to name; the raw Block must be preserved (compile-time injection only).
        let tool_result_node = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::BlockComplete { block } => match block.block() {
                    Block::ToolResult { .. } => Some(block.clone()),
                    _ => None,
                },
                _ => None,
            })
            .next()
            .ok_or("expected a ToolResult BlockComplete event")?;
        let expected_handle = tool_result_node.checkpoint_handle();
        match tool_result_node.block() {
            Block::ToolResult {
                result, is_error, ..
            } => {
                assert_eq!(result, &serde_json::json!("raw output"));
                assert!(!*is_error);
            }
            other => return Err(format!("expected ToolResult, got {other:?}").into()),
        }

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
                assert!(!*is_error);
                let rendered = content
                    .as_str()
                    .ok_or("expected tool result content to be a string")?;
                assert!(
                    rendered.starts_with("raw output"),
                    "original result must be preserved, got {rendered}"
                );
                let expected_line = format!("[pristine checkpoint] {expected_handle}");
                assert!(
                    rendered.contains(&expected_line),
                    "expected handle line {expected_line:?} in {rendered:?}"
                );
            }
            other => return Err(format!("expected ToolResult, got {other:?}").into()),
        }
        Ok(())
    }

    /// Drain `outbound` into `sink` until `n` Idle events have been observed.
    async fn drain_until_idles(
        outbound: &mut BoxStream<'static, AgentEvent>,
        sink: &mut Vec<AgentEvent>,
        n: usize,
    ) {
        let mut idles = 0;
        while let Some(evt) = outbound.next().await {
            let is_idle = matches!(evt, AgentEvent::Idle);
            sink.push(evt);
            if is_idle {
                idles += 1;
                if idles >= n {
                    return;
                }
            }
        }
    }

    #[tokio::test]
    async fn agent_run_dispatches_tool_call_and_re_enters_model()
    -> Result<(), Box<dyn std::error::Error>> {
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
            .system_prompt(SystemPrompt {
                base: "test prompt".to_string(),
                skills: None,
            })
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
            drain_until_idles(&mut outbound, &mut events, 1),
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
            other => return Err(format!("expected ToolUse, got {other:?}").into()),
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
                let rendered = content
                    .as_str()
                    .ok_or("expected tool result content to be a string")?;
                assert!(rendered.contains("\"echo\""));
                assert!(rendered.contains("\"text\""));
                assert!(rendered.contains("[pristine checkpoint] ckpt-"));
                assert!(!*is_error);
            }
            other => return Err(format!("expected ToolResult, got {other:?}").into()),
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
        Ok(())
    }

    #[tokio::test]
    async fn agent_run_dispatches_unknown_tool_yields_error_result()
    -> Result<(), Box<dyn std::error::Error>> {
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
            drain_until_idles(&mut outbound, &mut events, 1),
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
            other => return Err(format!("expected ToolResult, got {other:?}").into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn agent_run_publishes_reasoning_and_appends_trace_before_agent_message()
    -> Result<(), Box<dyn std::error::Error>> {
        let model = Arc::new(StubArModel::with_events(vec![
            Ok(ModelStreamEvent::ReasoningDelta {
                text: "let me ".to_string(),
            }),
            Ok(ModelStreamEvent::ReasoningDelta {
                text: "think".to_string(),
            }),
            Ok(ModelStreamEvent::ReasoningComplete {
                text: "let me think".to_string(),
            }),
            Ok(ModelStreamEvent::ContentComplete {
                text: "the answer".to_string(),
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

        bus.send_inbound(agent_id, user_block("question"))
            .expect("send inbound");

        let handle = tokio::spawn(agent.run());

        let mut events: Vec<AgentEvent> = Vec::new();
        timeout(
            Duration::from_secs(2),
            drain_until_idles(&mut outbound, &mut events, 1),
        )
        .await
        .expect("drain timed out");

        bus.close_inbound(agent_id);
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("join timed out")
            .expect("join")
            .expect("run ok");

        let reasoning_deltas: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ReasoningDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning_deltas, vec!["let me ", "think"]);

        let block_completes: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::BlockComplete { block } => Some(block.clone()),
                _ => None,
            })
            .collect();
        // user_message, reasoning_trace, agent_message
        assert_eq!(block_completes.len(), 3);
        match block_completes[1].block() {
            Block::ReasoningTrace { content, .. } => assert_eq!(content, "let me think"),
            other => return Err(format!("expected ReasoningTrace, got {other:?}").into()),
        }
        match block_completes[2].block() {
            Block::AgentMessage { content, .. } => assert_eq!(content, "the answer"),
            other => return Err(format!("expected AgentMessage, got {other:?}").into()),
        }

        let input = captured.last_input().expect("model called");
        let has_reasoning = input.turns.iter().any(|t| {
            t.content
                .iter()
                .any(|c| matches!(c, ContentPart::Text(s) if s == "let me think"))
        });
        assert!(
            !has_reasoning,
            "reasoning trace must not be compiled into ModelInput; turns: {:?}",
            input.turns
        );
        Ok(())
    }

    #[tokio::test]
    async fn agent_run_uses_reasoning_delta_buffer_when_no_reasoning_complete()
    -> Result<(), Box<dyn std::error::Error>> {
        let model = Arc::new(StubArModel::with_events(vec![
            Ok(ModelStreamEvent::ReasoningDelta {
                text: "partial reasoning".to_string(),
            }),
            Ok(ModelStreamEvent::ContentComplete {
                text: "answer".to_string(),
            }),
            Ok(ModelStreamEvent::MessageComplete {
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            }),
        ]));
        let (agent, agent_id, bus, mut outbound) = build_agent(model);

        bus.send_inbound(agent_id, user_block("question"))
            .expect("send inbound");

        let handle = tokio::spawn(agent.run());

        let mut events: Vec<AgentEvent> = Vec::new();
        timeout(
            Duration::from_secs(2),
            drain_until_idles(&mut outbound, &mut events, 1),
        )
        .await
        .expect("drain timed out");

        bus.close_inbound(agent_id);
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("join timed out")
            .expect("join")
            .expect("run ok");

        let reasoning_trace = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::BlockComplete { block } => match block.block() {
                    Block::ReasoningTrace { content, .. } => Some(content.clone()),
                    _ => None,
                },
                _ => None,
            })
            .next()
            .expect("at least one ReasoningTrace block");
        assert_eq!(reasoning_trace, "partial reasoning");
        Ok(())
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
            .system_prompt(SystemPrompt {
                base: "test prompt".to_string(),
                skills: None,
            })
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
            drain_until_idles(&mut outbound, &mut events, 1),
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

    #[tokio::test]
    async fn fork_request_control_spawns_peer_without_model_turn()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::messagebus::{Control, ForkRequest};
        use crate::test_support::RecordingSpawner;

        // Seed the parent with a history prefix so the fork inherits it.
        let mut prefix = History::new();
        let head = prefix.append(user_block("seeded context"));

        let model = Arc::new(StubArModel::empty());
        let captured = model.clone();
        let spawner = Arc::new(RecordingSpawner::new());

        let agent_id = AgentId::new();
        let bus = Arc::new(InMemoryMessageBus::new());
        let agent = AgentBuilder::new()
            .id(agent_id)
            .system_prompt(SystemPrompt {
                base: "parent".to_string(),
                skills: None,
            })
            .model(ModelRole::Default, model as Arc<dyn ARModel>)
            .history_prefix(Some(head.clone()))
            .spawner(spawner.clone())
            .build(bus.clone() as Arc<dyn MessageBus>)
            .expect("build agent");

        let handle = tokio::spawn(agent.run());

        bus.send_control(
            agent_id,
            Control::Fork(ForkRequest {
                instruction: "do it".to_string(),
                handle: None,
                tools: None,
            }),
        )
        .expect("send control");

        // Closing inbound ends the run loop after the buffered control message
        // drains, so join is a deterministic barrier for the fork having run.
        bus.close_inbound(agent_id);
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("join timed out")
            .expect("join")
            .expect("run ok");

        let specs = spawner.specs();
        assert_eq!(specs.len(), 1, "one peer must be spawned");
        let prefix_head = specs[0]
            .history_prefix
            .as_ref()
            .ok_or("omitted handle inherits the full prefix")?;
        assert_eq!(
            prefix_head.id(),
            head.id(),
            "fork inherits the parent's live head",
        );
        match &specs[0].instruction {
            Some(Block::UserMessage { content, .. }) => assert_eq!(content, "do it"),
            other => return Err(format!("expected a seeded instruction, got {other:?}").into()),
        }
        assert_eq!(
            specs[0].origin,
            Some(agent_id),
            "fork records its originating agent",
        );

        assert!(
            captured.last_input().is_none(),
            "the fork request must not trigger a model turn on the parent",
        );
        Ok(())
    }
}
