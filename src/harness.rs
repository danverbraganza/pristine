//! Harness lifecycle and registry.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use futures::stream::BoxStream;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentBuilder, SystemPrompt};
use crate::history::{AgentId, Block};
use crate::messagebus::{AgentEvent, InMemoryMessageBus, MessageBus};
use crate::model::{ARModel, ModelRole};
use crate::provider::{ModelProvider, ProviderError, ProviderRegistry};
use crate::tool::{Tool, ToolError, ToolRegistry};
use crate::user::{User, UserId};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug)]
pub struct PendingAgent {
    pub id: AgentId,
    pub system_prompt: SystemPrompt,
    pub model_id: ModelId,
}

#[derive(Debug)]
pub enum Error {
    UnknownModel(ModelId),
    UnknownAgent(AgentId),
    Lifecycle(String),
    Bus(crate::messagebus::Error),
    Join(String),
    DuplicateTool(String),
    DuplicateProvider(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnknownModel(id) => write!(f, "unknown model: {id}"),
            Error::UnknownAgent(id) => write!(f, "unknown agent: {id}"),
            Error::Lifecycle(msg) => write!(f, "lifecycle error: {msg}"),
            Error::Bus(err) => write!(f, "message bus error: {err}"),
            Error::Join(msg) => write!(f, "task join error: {msg}"),
            Error::DuplicateTool(name) => write!(f, "duplicate tool: {name}"),
            Error::DuplicateProvider(name) => write!(f, "duplicate provider: {name}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Bus(err) => Some(err),
            Error::DuplicateTool(_) => None,
            Error::DuplicateProvider(_) => None,
            _ => None,
        }
    }
}

impl From<crate::messagebus::Error> for Error {
    fn from(err: crate::messagebus::Error) -> Self {
        Error::Bus(err)
    }
}

impl From<tokio::task::JoinError> for Error {
    fn from(err: tokio::task::JoinError) -> Self {
        Error::Join(err.to_string())
    }
}

struct AgentHandle {
    task: JoinHandle<Result<(), Error>>,
}

pub struct Harness {
    models: HashMap<ModelId, Arc<dyn ARModel>>,
    agents: HashMap<AgentId, AgentHandle>,
    bus: Arc<InMemoryMessageBus>,
    owner: User,
    cancel: CancellationToken,
    pending: Vec<PendingAgent>,
    tools: Arc<ToolRegistry>,
    provider_registry: Arc<ProviderRegistry>,
}

pub struct HarnessBuilder {
    models: HashMap<ModelId, Arc<dyn ARModel>>,
    pending: Vec<PendingAgent>,
    tools: Arc<ToolRegistry>,
    provider_registry: Arc<ProviderRegistry>,
}

impl HarnessBuilder {
    pub fn new() -> Self {
        Self {
            models: HashMap::new(),
            pending: Vec::new(),
            tools: Arc::new(ToolRegistry::new()),
            provider_registry: Arc::new(ProviderRegistry::new()),
        }
    }

    pub fn add_model(mut self, id: ModelId, model: Arc<dyn ARModel>) -> Self {
        self.models.insert(id, model);
        self
    }

    pub fn add_agent(mut self, agent: PendingAgent) -> Self {
        self.pending.push(agent);
        self
    }

    pub fn add_tool(mut self, tool: Arc<dyn Tool>) -> Result<Self, Error> {
        let registry = Arc::get_mut(&mut self.tools).ok_or_else(|| {
            Error::Lifecycle("tool registry has been shared; cannot mutate".to_string())
        })?;
        registry.register(tool).map_err(|e| match e {
            ToolError::AlreadyRegistered(name) => Error::DuplicateTool(name),
            other => Error::Lifecycle(other.to_string()),
        })?;
        Ok(self)
    }

    pub fn add_provider(
        mut self,
        name: impl Into<String>,
        provider: Arc<dyn ModelProvider>,
    ) -> Result<Self, Error> {
        let registry = Arc::get_mut(&mut self.provider_registry).ok_or_else(|| {
            Error::Lifecycle("provider registry has been shared; cannot mutate".to_string())
        })?;
        registry.add(name, provider).map_err(|e| match e {
            ProviderError::DuplicateProvider(name) => Error::DuplicateProvider(name),
            other => Error::Lifecycle(other.to_string()),
        })?;
        Ok(self)
    }

    pub fn build(self) -> Result<Harness, Error> {
        for pending in &self.pending {
            if !self.models.contains_key(&pending.model_id) {
                return Err(Error::UnknownModel(pending.model_id.clone()));
            }
        }
        Ok(Harness {
            models: self.models,
            agents: HashMap::new(),
            bus: Arc::new(InMemoryMessageBus::new()),
            owner: User::new(),
            cancel: CancellationToken::new(),
            pending: self.pending,
            tools: self.tools,
            provider_registry: self.provider_registry,
        })
    }
}

impl Default for HarnessBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl Harness {
    pub fn owner_id(&self) -> UserId {
        self.owner.id()
    }

    /// Synchronously spawn per-Agent tasks and MessageBus routing tasks.
    pub fn start(&mut self) -> Result<(), Error> {
        let pending = std::mem::take(&mut self.pending);
        for spec in pending {
            let agent_id = spec.id;
            let model = self
                .models
                .get(&spec.model_id)
                .ok_or_else(|| Error::UnknownModel(spec.model_id.clone()))?
                .clone();
            let bus_dyn: Arc<dyn MessageBus> = self.bus.clone();
            let agent = AgentBuilder::new()
                .id(agent_id)
                .system_prompt(spec.system_prompt)
                .model(ModelRole::Default, model)
                .tools(self.tools.clone())
                .build(bus_dyn)
                .map_err(|e| Error::Lifecycle(e.to_string()))?;
            let cancel = self.cancel.clone();
            let bus_for_emit = self.bus.clone();
            let task: JoinHandle<Result<(), Error>> = tokio::spawn(async move {
                let res: Result<(), Error> = tokio::select! {
                    _ = cancel.cancelled() => Ok(()),
                    res = agent.run() => res.map_err(|e| Error::Lifecycle(e.to_string())),
                };
                if let Err(ref e) = res {
                    let _ = bus_for_emit.publish(
                        agent_id,
                        AgentEvent::Error {
                            message: e.to_string(),
                        },
                    );
                }
                res
            });
            self.agents.insert(agent_id, AgentHandle { task });
        }
        Ok(())
    }

    /// Await every spawned task; returns the first error encountered. Idempotent.
    pub async fn join(&mut self) -> Result<(), Error> {
        // Drain the registry before awaiting so the `&mut self` borrow is
        // released across each await.
        let handles: Vec<JoinHandle<Result<(), Error>>> = std::mem::take(&mut self.agents)
            .into_values()
            .map(|h| h.task)
            .collect();
        let mut first_err: Option<Error> = None;
        for handle in handles {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(join_err) => {
                    if first_err.is_none() {
                        first_err = Some(Error::from(join_err));
                    }
                }
            }
        }
        // Clean up ExecBash's per-process tmp directory if it was created.
        // Cleanup failure does not fail the join — a leaked tmp dir is
        // undesirable but recoverable; failing join would mask the real
        // shutdown result.
        if let Err(e) = crate::builtins::exec_bash::cleanup_tmp_dir() {
            eprintln!("warning: failed to clean up ExecBash tmp directory: {e}");
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Signal cooperative termination to every spawned task. Idempotent.
    pub fn shutdown(&mut self) {
        self.cancel.cancel();
    }

    pub fn send_to_agent(
        &self,
        agent_id: AgentId,
        from: UserId,
        content: String,
    ) -> Result<(), Error> {
        let block = Block::UserMessage {
            from,
            content,
            timestamp: SystemTime::now(),
        };
        self.bus.send_inbound(agent_id, block)?;
        Ok(())
    }

    pub fn subscribe(&self, agent_id: AgentId) -> Result<BoxStream<'static, AgentEvent>, Error> {
        Ok(self.bus.subscribe(agent_id)?)
    }

    pub fn bus(&self) -> &Arc<InMemoryMessageBus> {
        &self.bus
    }

    pub fn tools(&self) -> &Arc<ToolRegistry> {
        &self.tools
    }

    pub fn provider_registry(&self) -> &Arc<ProviderRegistry> {
        &self.provider_registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use futures::StreamExt;
    use tokio::time::timeout;

    use crate::builtins::ExecBash;
    use crate::messagebus::AgentEvent;
    use crate::model::Error as ModelError;
    use crate::shell::{ExecStatus, ShellOutput};
    use crate::test_support::{EchoTool, StubArModel, StubShell};
    use crate::tool::Tool;
    use serde_json::json;

    fn prompt(base: &str) -> SystemPrompt {
        SystemPrompt {
            base: base.to_string(),
            skills: None,
        }
    }

    fn build_harness_with_one_agent() -> (Harness, AgentId, ModelId) {
        let model_id = ModelId::new("stub");
        let agent_id = AgentId::new();
        let harness = HarnessBuilder::new()
            .add_model(model_id.clone(), Arc::new(StubArModel::empty()))
            .add_agent(PendingAgent {
                id: agent_id,
                system_prompt: prompt("test"),
                model_id: model_id.clone(),
            })
            .build()
            .expect("build harness");
        (harness, agent_id, model_id)
    }

    #[test]
    fn builder_rejects_agent_with_unknown_model() -> Result<(), Box<dyn std::error::Error>> {
        let missing = ModelId::new("missing");
        let agent_id = AgentId::new();
        let result = HarnessBuilder::new()
            .add_agent(PendingAgent {
                id: agent_id,
                system_prompt: prompt("test"),
                model_id: missing.clone(),
            })
            .build();
        match result {
            Err(Error::UnknownModel(id)) => assert_eq!(id, missing),
            Err(other) => return Err(format!("expected UnknownModel, got {other:?}").into()),
            Ok(_) => return Err("builder should reject unknown model".into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn start_then_shutdown_exits_cleanly() {
        let (mut harness, _agent_id, _model_id) = build_harness_with_one_agent();
        harness.start().expect("start");
        harness.shutdown();
        let result = timeout(Duration::from_secs(5), harness.join())
            .await
            .expect("join timed out");
        result.expect("clean shutdown");
    }

    #[tokio::test]
    async fn send_to_agent_routes_through_bus() {
        let (mut harness, agent_id, _model_id) = build_harness_with_one_agent();
        harness.start().expect("start");

        let owner = harness.owner_id();
        harness
            .send_to_agent(agent_id, owner, "hello".to_string())
            .expect("send_to_agent");

        // This test verifies the inbound-routing happy path through the bus.
        // Driving the full agent->model->event cycle is covered by the agent module tests.
        harness.shutdown();
        timeout(Duration::from_secs(5), harness.join())
            .await
            .expect("join timed out")
            .expect("clean shutdown");
    }

    #[tokio::test]
    async fn subscribe_returns_receiver_seeing_published_event()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut harness, agent_id, _model_id) = build_harness_with_one_agent();
        harness.start().expect("start");

        let mut sub = harness.subscribe(agent_id).expect("subscribe");
        harness
            .bus()
            .publish(
                agent_id,
                AgentEvent::TokenDelta {
                    text: "hi".to_string(),
                },
            )
            .expect("publish token delta");

        let event = timeout(Duration::from_secs(1), sub.next())
            .await
            .expect("recv timed out")
            .expect("stream closed");
        match event {
            AgentEvent::TokenDelta { text } => assert_eq!(text, "hi"),
            other => return Err(format!("expected TokenDelta, got {other:?}").into()),
        }

        harness.shutdown();
        timeout(Duration::from_secs(5), harness.join())
            .await
            .expect("join timed out")
            .expect("clean shutdown");
        Ok(())
    }

    #[tokio::test]
    async fn harness_emits_error_event_when_agent_dies() {
        let model_id = ModelId::new("stub");
        let agent_id = AgentId::new();
        let model = Arc::new(StubArModel::with_events(vec![Err(ModelError::Api {
            status: 500,
            message: "boom".to_string(),
        })]));
        let mut harness = HarnessBuilder::new()
            .add_model(model_id.clone(), model)
            .add_agent(PendingAgent {
                id: agent_id,
                system_prompt: prompt("test"),
                model_id: model_id.clone(),
            })
            .build()
            .expect("build harness");
        harness.start().expect("start");

        let mut sub = harness.subscribe(agent_id).expect("subscribe");
        let owner = harness.owner_id();
        harness
            .send_to_agent(agent_id, owner, "trigger".to_string())
            .expect("send_to_agent");

        let mut error_message: Option<String> = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while error_message.is_none() && tokio::time::Instant::now() < deadline {
            match timeout(Duration::from_secs(2), sub.next()).await {
                Ok(Some(AgentEvent::Error { message })) => error_message = Some(message),
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => break,
            }
        }
        let message = error_message.expect("expected Error event");
        assert!(message.contains("500"), "missing status: {message}");
        assert!(message.contains("boom"), "missing inner message: {message}");

        // The agent task has already exited, so join() returns without a
        // prior shutdown(). It propagates the agent's error.
        let join_no_shutdown = timeout(Duration::from_secs(2), harness.join())
            .await
            .expect("join without shutdown timed out");
        assert!(
            join_no_shutdown.is_err(),
            "expected agent error to surface via join, got Ok"
        );

        // A subsequent shutdown() + join() is a no-op (agent already drained).
        harness.shutdown();
        timeout(Duration::from_secs(2), harness.join())
            .await
            .expect("second join timed out")
            .expect("idempotent join after drained registry");
    }

    #[tokio::test]
    async fn multi_agent_failure_isolation() {
        let model_id_a = ModelId::new("stub-a");
        let model_id_b = ModelId::new("stub-b");
        let agent_a = AgentId::new();
        let agent_b = AgentId::new();
        let model_a = Arc::new(StubArModel::with_events(vec![Err(ModelError::Api {
            status: 500,
            message: "boom".to_string(),
        })]));
        let model_b = Arc::new(StubArModel::empty());
        let mut harness = HarnessBuilder::new()
            .add_model(model_id_a.clone(), model_a)
            .add_model(model_id_b.clone(), model_b)
            .add_agent(PendingAgent {
                id: agent_a,
                system_prompt: prompt("a"),
                model_id: model_id_a,
            })
            .add_agent(PendingAgent {
                id: agent_b,
                system_prompt: prompt("b"),
                model_id: model_id_b,
            })
            .build()
            .expect("build harness");
        harness.start().expect("start");

        let mut sub_a = harness.subscribe(agent_a).expect("subscribe a");
        let mut sub_b = harness.subscribe(agent_b).expect("subscribe b");
        let owner = harness.owner_id();
        harness
            .send_to_agent(agent_a, owner, "trigger".to_string())
            .expect("send_to_agent");

        let mut error_seen = false;
        timeout(Duration::from_secs(2), async {
            while let Some(evt) = sub_a.next().await {
                if let AgentEvent::Error { .. } = evt {
                    error_seen = true;
                    break;
                }
            }
        })
        .await
        .expect("agent A error wait timed out");
        assert!(error_seen, "expected AgentEvent::Error from agent A");

        // Agent B is still running and has emitted no events: a brief poll
        // times out without yielding anything.
        let poll = timeout(Duration::from_millis(100), sub_b.next()).await;
        assert!(
            poll.is_err(),
            "agent B unexpectedly emitted event or closed: {poll:?}"
        );

        // Shutdown signals B to exit; join then collects both tasks. The
        // first error (A's) propagates; B returns Ok.
        harness.shutdown();
        let result = timeout(Duration::from_secs(2), harness.join())
            .await
            .expect("join timed out");
        assert!(
            result.is_err(),
            "expected agent A's failure to propagate from join"
        );
    }

    #[test]
    fn harness_builder_starts_with_empty_tool_registry() {
        let (harness, _agent_id, _model_id) = build_harness_with_one_agent();
        assert!(harness.tools().list().is_empty());
    }

    #[test]
    fn harness_builder_add_tool_makes_tool_visible_to_agents() {
        let model_id = ModelId::new("stub");
        let agent_id = AgentId::new();
        let harness = HarnessBuilder::new()
            .add_model(model_id.clone(), Arc::new(StubArModel::empty()))
            .add_agent(PendingAgent {
                id: agent_id,
                system_prompt: prompt("test"),
                model_id,
            })
            .add_tool(Arc::new(EchoTool::new("echo")))
            .expect("add_tool succeeds")
            .build()
            .expect("build harness");
        assert!(harness.tools().get("echo").is_some());
    }

    #[tokio::test]
    async fn harness_join_cleans_up_exec_bash_tmp_dir() {
        // Prime TMP_DIR by invoking ExecBash directly via a stub shell. This
        // populates the static OnceLock and creates the directory on disk
        // without going through the harness/agent flow.
        let stub = Arc::new(StubShell::new(vec![Ok(ShellOutput {
            stdout: b"hi".to_vec(),
            stderr: Vec::new(),
            status: ExecStatus::Exit { code: 0 },
        })]));
        let prime_tool = ExecBash::with_shell(stub);
        prime_tool
            .call(json!({"command": "true"}))
            .await
            .expect("priming exec_bash call succeeds");

        let expected = std::env::temp_dir().join(format!("pristine-{}", std::process::id()));
        assert!(
            expected.exists(),
            "expected tmp dir {expected:?} to exist after priming",
        );

        // Build a minimal harness with ExecBash registered but no message ever
        // sent. The harness lifecycle alone should clean up TMP_DIR on join.
        let (mut harness, _agent_id, _model_id) = build_harness_with_one_agent();
        harness.start().expect("start");
        harness.shutdown();
        timeout(Duration::from_secs(5), harness.join())
            .await
            .expect("join timed out")
            .expect("clean shutdown");

        assert!(
            !expected.exists(),
            "expected tmp dir {expected:?} to be removed after harness join",
        );
    }

    #[test]
    fn harness_builder_duplicate_tool_returns_error() -> Result<(), Box<dyn std::error::Error>> {
        let builder = HarnessBuilder::new()
            .add_tool(Arc::new(EchoTool::new("echo")))
            .expect("first add_tool succeeds");
        let result = builder.add_tool(Arc::new(EchoTool::new("echo")));
        match result {
            Err(Error::DuplicateTool(name)) => assert_eq!(name, "echo"),
            Err(other) => return Err(format!("expected DuplicateTool, got {other:?}").into()),
            Ok(_) => return Err("second add_tool should fail".into()),
        }
        Ok(())
    }

    struct StubProvider;

    impl crate::provider::ModelProvider for StubProvider {
        fn build_model(
            &self,
            _config: crate::provider::ModelInstanceConfig,
        ) -> Result<Arc<dyn crate::model::ARModel>, crate::provider::ProviderError> {
            Ok(Arc::new(StubArModel::empty()))
        }
    }

    #[test]
    fn harness_builder_add_provider_makes_provider_visible() {
        let harness = HarnessBuilder::new()
            .add_provider("stub", Arc::new(StubProvider))
            .expect("add_provider succeeds")
            .build()
            .expect("build harness");
        assert!(harness.provider_registry().get("stub").is_some());
    }

    #[test]
    fn harness_builder_duplicate_provider_returns_error() -> Result<(), Box<dyn std::error::Error>>
    {
        let builder = HarnessBuilder::new()
            .add_provider("stub", Arc::new(StubProvider))
            .expect("first add_provider succeeds");
        let result = builder.add_provider("stub", Arc::new(StubProvider));
        match result {
            Err(Error::DuplicateProvider(name)) => assert_eq!(name, "stub"),
            Err(other) => return Err(format!("expected DuplicateProvider, got {other:?}").into()),
            Ok(_) => return Err("second add_provider should fail".into()),
        }
        Ok(())
    }
}
