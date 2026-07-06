//! Harness lifecycle and registry.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::SystemTime;

use futures::stream::BoxStream;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentBuilder, SystemPrompt};
use crate::history::{AgentId, Block, CheckpointHandle, HistoryNode};
use crate::messagebus::{AgentEvent, AgentForked, InMemoryMessageBus, MessageBus};
use crate::model::{ARModel, ModelRole};
use crate::provider::{ModelProvider, ProviderError, ProviderRegistry};
use crate::rpc::{SkillsLoadedNotification, SkillsNotification};
use crate::skills::SkillsRegistrySource;
use crate::tool::{Tool, ToolError, ToolRegistry};
use crate::user::{User, UserId};

/// One-shot emitter for the session-level `skills_loaded` /
/// `skills_diagnostics` notifications.
///
/// Discovery is lazy-strict: the first system-prompt render triggers the
/// registry's filesystem scan. This announcer is the seam that surfaces that
/// outcome on the JSON-RPC notification surface exactly once. The transport
/// (`crate::stdio`) calls [`take`](SkillsAnnouncer::take) after the first
/// `send_message`-triggered turn; the `emitted` guard makes a multi-turn agent
/// loop unable to re-emit.
pub struct SkillsAnnouncer {
    source: Arc<dyn SkillsRegistrySource>,
    emitted: AtomicBool,
}

impl SkillsAnnouncer {
    /// Wrap a skills source for one-shot announcement.
    pub fn new(source: Arc<dyn SkillsRegistrySource>) -> Self {
        Self {
            source,
            emitted: AtomicBool::new(false),
        }
    }

    /// Drain the discovery outcome into the notifications to emit, exactly once.
    ///
    /// Returns the notifications on the first call and an empty `Vec` on every
    /// subsequent call. Triggers the registry's lazy scan if it has not run.
    ///
    /// Omission rules (requirements §6.2): a `skills_loaded` notification is
    /// emitted only when the catalog is non-empty; `skills_diagnostics` only
    /// when there is at least one diagnostic. When discovery found nothing and
    /// produced no diagnostics, nothing is emitted.
    pub fn take(&self) -> Vec<SkillsNotification> {
        if self.emitted.swap(true, Ordering::SeqCst) {
            return Vec::new();
        }
        let mut notifications = Vec::new();
        let skills = self.source.list();
        if !skills.is_empty() {
            notifications.push(SkillsNotification::Loaded(SkillsLoadedNotification {
                skills,
            }));
        }
        let diagnostics = self.source.diagnostics();
        if !diagnostics.is_empty() {
            notifications.push(SkillsNotification::Diagnostics(diagnostics));
        }
        notifications
    }
}

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
    /// The Agent's per-agent cancellation child token. Cancelling it terminates
    /// only this Agent; the parent token (fired by `Harness::shutdown`) cancels
    /// every child at once.
    stop: CancellationToken,
}

/// [`AgentSpawner`] that refuses to spawn. Handed to Agents built outside a
/// running Harness (e.g. `AgentBuilder` in tests) so their [`ToolCallContext`]
/// always exposes a spawner; any spawn attempt surfaces as a clear error rather
/// than a panic.
pub(crate) struct DisconnectedSpawner;

impl AgentSpawner for DisconnectedSpawner {
    fn spawn(&self, _spec: AgentSpec) -> Result<AgentId, Error> {
        Err(Error::Lifecycle(
            "agent has no spawner attached".to_string(),
        ))
    }
}

/// Runtime specification for spawning a new peer Agent.
///
/// Carries everything the [`Nursery`] needs to build and register an Agent
/// after the Harness is already running: the system prompt, the model to reuse
/// (typically the initiator's), the tool set (the shared registry or a narrowed
/// one), an optional inherited history prefix head, and an optional initial
/// inbound instruction delivered once the Agent has registered.
pub struct AgentSpec {
    pub system_prompt: SystemPrompt,
    pub model: Arc<dyn ARModel>,
    pub tools: Arc<ToolRegistry>,
    pub history_prefix: Option<Arc<HistoryNode>>,
    pub instruction: Option<Block>,
    /// The originating Agent when this spec is a fork, else `None`. Paired with
    /// [`forked_from`](AgentSpec::forked_from), it drives the spawner path's
    /// uniform agent-forked event; a plain (non-fork) runtime spawn leaves both
    /// `None` and emits nothing.
    pub origin: Option<AgentId>,
    /// The checkpoint handle the fork inherited history up to, when this spec is
    /// a fork.
    pub forked_from: Option<CheckpointHandle>,
}

/// Read-only seam for spawning an Agent at runtime.
///
/// Follows the concrete-type-plus-trait shape of the engine's other seams
/// (`Tool`/`ToolRegistry`, `ModelProvider`/`ProviderRegistry`,
/// `SkillsRegistrySource`/`SkillsRegistry`): [`Nursery`] is the engine-owned
/// concrete implementor, and this trait is the abstraction a running Agent's
/// tools resolve against to spawn a peer.
pub trait AgentSpawner: Send + Sync {
    /// Spawn a new Agent from `spec`, returning its freshly allocated id.
    fn spawn(&self, spec: AgentSpec) -> Result<AgentId, Error>;
}

/// The Harness-owned concrete [`AgentSpawner`].
///
/// Owns the shared task registry so build-time spawning (`Harness::start`
/// draining `PendingAgent`s) and runtime spawning ([`AgentSpawner::spawn`] from
/// another task) funnel through one routine, [`build_and_track`](Nursery::build_and_track).
/// Every spawned Agent registers with the shared `MessageBus`, observes the
/// shared `CancellationToken`, and has its `JoinHandle` tracked so
/// `Harness::join` awaits it and `Harness::shutdown` cancels it. The registry is
/// behind a `Mutex` because runtime spawns arrive from tasks other than the one
/// driving the Harness lifecycle.
pub struct Nursery {
    bus: Arc<InMemoryMessageBus>,
    cancel: CancellationToken,
    agents: Mutex<HashMap<AgentId, AgentHandle>>,
    /// A weak self-reference so `build_and_track` can hand each Agent an
    /// `Arc<dyn AgentSpawner>` pointing back at this Nursery. Set once at
    /// construction via `Arc::new_cyclic`; it upgrades for the Nursery's whole
    /// lifetime because the Harness owns the strong `Arc`.
    me: Weak<Nursery>,
}

impl Nursery {
    fn new(bus: Arc<InMemoryMessageBus>, cancel: CancellationToken, me: Weak<Nursery>) -> Self {
        Self {
            bus,
            cancel,
            agents: Mutex::new(HashMap::new()),
            me,
        }
    }

    /// Build, spawn, and track one Agent task. The single spawn routine shared
    /// by the build-time and runtime paths.
    fn build_and_track(
        &self,
        agent_id: AgentId,
        system_prompt: SystemPrompt,
        model: Arc<dyn ARModel>,
        tools: Arc<ToolRegistry>,
        history_prefix: Option<Arc<HistoryNode>>,
    ) -> Result<(), Error> {
        let bus_dyn: Arc<dyn MessageBus> = self.bus.clone();
        let child = self.cancel.child_token();
        let spawner: Arc<dyn AgentSpawner> = self
            .me
            .upgrade()
            .ok_or_else(|| Error::Lifecycle("nursery has been dropped".to_string()))?;
        let agent = AgentBuilder::new()
            .id(agent_id)
            .system_prompt(system_prompt)
            .model(ModelRole::Default, model)
            .tools(tools)
            .history_prefix(history_prefix)
            .spawner(spawner)
            .stop_token(child.clone())
            .build(bus_dyn)
            .map_err(|e| Error::Lifecycle(e.to_string()))?;
        let cancel = child.clone();
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
        let mut agents = self
            .agents
            .lock()
            .map_err(|_| Error::Lifecycle("agent registry mutex poisoned".to_string()))?;
        agents.insert(agent_id, AgentHandle { task, stop: child });
        Ok(())
    }

    /// Terminate one tracked Agent by cancelling its per-agent child token,
    /// leaving every sibling running. The task exits cleanly on its next
    /// cancellation-observing await; `Harness::join` later collects it. Returns
    /// [`Error::UnknownAgent`] if no Agent with `agent_id` is tracked.
    pub fn stop(&self, agent_id: AgentId) -> Result<(), Error> {
        let agents = self
            .agents
            .lock()
            .map_err(|_| Error::Lifecycle("agent registry mutex poisoned".to_string()))?;
        let handle = agents.get(&agent_id).ok_or(Error::UnknownAgent(agent_id))?;
        handle.stop.cancel();
        Ok(())
    }

    /// Drain the tracked task handles, leaving the registry empty.
    fn drain(&self) -> Result<Vec<JoinHandle<Result<(), Error>>>, Error> {
        let mut agents = self
            .agents
            .lock()
            .map_err(|_| Error::Lifecycle("agent registry mutex poisoned".to_string()))?;
        Ok(std::mem::take(&mut *agents)
            .into_values()
            .map(|handle| handle.task)
            .collect())
    }
}

impl AgentSpawner for Nursery {
    fn spawn(&self, spec: AgentSpec) -> Result<AgentId, Error> {
        let agent_id = AgentId::new();
        let origin = spec.origin;
        let forked_from = spec.forked_from;
        self.build_and_track(
            agent_id,
            spec.system_prompt,
            spec.model,
            spec.tools,
            spec.history_prefix,
        )?;
        if let Some(block) = spec.instruction {
            self.bus.send_inbound(agent_id, block)?;
        }
        // A fork carries both its origin and inherited handle; emit the uniform
        // agent-forked event so tool- and control-path forks surface alike.
        if let (Some(origin), Some(handle)) = (origin, forked_from) {
            let _ = self.bus.publish_fork(AgentForked {
                agent_id,
                origin,
                handle,
            });
        }
        Ok(agent_id)
    }
}

pub struct Harness {
    models: HashMap<ModelId, Arc<dyn ARModel>>,
    nursery: Arc<Nursery>,
    bus: Arc<InMemoryMessageBus>,
    owner: User,
    cancel: CancellationToken,
    pending: Vec<PendingAgent>,
    tools: Arc<ToolRegistry>,
    provider_registry: Arc<ProviderRegistry>,
    skills: Option<Arc<SkillsAnnouncer>>,
}

pub struct HarnessBuilder {
    models: HashMap<ModelId, Arc<dyn ARModel>>,
    pending: Vec<PendingAgent>,
    tools: Arc<ToolRegistry>,
    provider_registry: Arc<ProviderRegistry>,
    skills: Option<Arc<dyn SkillsRegistrySource>>,
}

impl HarnessBuilder {
    pub fn new() -> Self {
        Self {
            models: HashMap::new(),
            pending: Vec::new(),
            tools: Arc::new(ToolRegistry::new()),
            provider_registry: Arc::new(ProviderRegistry::new()),
            skills: None,
        }
    }

    /// Wire the shared skills source whose discovery outcome is announced once
    /// on the JSON-RPC notification surface. Omitted when skills are disabled.
    pub fn skills(mut self, source: Arc<dyn SkillsRegistrySource>) -> Self {
        self.skills = Some(source);
        self
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
        let bus = Arc::new(InMemoryMessageBus::new());
        let cancel = CancellationToken::new();
        let nursery = Arc::new_cyclic(|me| Nursery::new(bus.clone(), cancel.clone(), me.clone()));
        Ok(Harness {
            models: self.models,
            nursery,
            bus,
            owner: User::new(),
            cancel,
            pending: self.pending,
            tools: self.tools,
            provider_registry: self.provider_registry,
            skills: self
                .skills
                .map(|source| Arc::new(SkillsAnnouncer::new(source))),
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
            let model = self
                .models
                .get(&spec.model_id)
                .ok_or_else(|| Error::UnknownModel(spec.model_id.clone()))?
                .clone();
            self.nursery.build_and_track(
                spec.id,
                spec.system_prompt,
                model,
                self.tools.clone(),
                None,
            )?;
        }
        Ok(())
    }

    /// Await every spawned task; returns the first error encountered. Idempotent.
    pub async fn join(&mut self) -> Result<(), Error> {
        // Drain the registry before awaiting so no lock is held across an await.
        let handles = self.nursery.drain()?;
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

    /// The runtime agent spawner. Handed to tools that spawn peer Agents (e.g.
    /// forking) so they can register a new Agent while the Harness runs; the
    /// returned handle shares the Harness's bus, cancellation token, and task
    /// registry.
    pub fn spawner(&self) -> Arc<dyn AgentSpawner> {
        self.nursery.clone()
    }

    /// The one-shot skills announcer, present iff skills are enabled. Handed to
    /// the transport so the discovery outcome surfaces once on the notification
    /// surface after the first agent turn.
    pub fn skills_announcer(&self) -> Option<Arc<SkillsAnnouncer>> {
        self.skills.clone()
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
    fn skills_announcer_emits_loaded_once_with_payload() -> Result<(), Box<dyn std::error::Error>> {
        use crate::skills::SkillRecord;
        use crate::test_support::StubSkillsRegistry;

        let source: Arc<dyn SkillsRegistrySource> =
            Arc::new(StubSkillsRegistry::new(vec![SkillRecord {
                name: "alpha".to_string(),
                description: "first skill".to_string(),
                body: String::new(),
                directory: std::path::PathBuf::from("/tmp/alpha"),
            }]));
        let announcer = SkillsAnnouncer::new(source);

        let first = announcer.take();
        assert_eq!(
            first.len(),
            1,
            "expected only skills_loaded (no diagnostics)"
        );
        match &first[0] {
            SkillsNotification::Loaded(loaded) => {
                assert_eq!(loaded.skills.len(), 1);
                assert_eq!(loaded.skills[0].name, "alpha");
                assert_eq!(loaded.skills[0].description, "first skill");
            }
            other => return Err(format!("expected Loaded, got method {}", other.method()).into()),
        }
        assert_eq!(announcer.take().len(), 0, "second take must be empty");
        Ok(())
    }

    #[test]
    fn skills_announcer_emits_diagnostics_for_malformed_skill()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::config::ResolvedSkillsConfig;
        use crate::skills::SkillsRegistry;
        use crate::test_support::SkillsFixture;

        // A SKILL.md with unterminated frontmatter yields a parse diagnostic.
        let fixture = SkillsFixture::new()?.add_raw_skill("broken", "---\nname: broken\n")?;
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
        let source: Arc<dyn SkillsRegistrySource> = Arc::new(SkillsRegistry::new(config, false));
        let announcer = SkillsAnnouncer::new(source);

        let notifications = announcer.take();
        let diagnostics = notifications
            .iter()
            .find_map(|n| match n {
                SkillsNotification::Diagnostics(d) => Some(d),
                _ => None,
            })
            .ok_or("expected a skills_diagnostics notification")?;
        assert!(!diagnostics.is_empty(), "expected at least one diagnostic");
        // Kind-tagged serialization carries a `kind` discriminator.
        let value = serde_json::to_value(diagnostics)?;
        assert!(
            value[0].get("kind").is_some(),
            "diagnostic must serialize with a kind tag: {value}"
        );
        // No skills survived discovery, so no skills_loaded is emitted.
        assert!(
            notifications
                .iter()
                .all(|n| !matches!(n, SkillsNotification::Loaded(_))),
            "empty catalog must not emit skills_loaded"
        );
        Ok(())
    }

    #[test]
    fn skills_announcer_emits_nothing_when_empty() -> Result<(), Box<dyn std::error::Error>> {
        use crate::test_support::StubSkillsRegistry;

        let source: Arc<dyn SkillsRegistrySource> = Arc::new(StubSkillsRegistry::new(vec![]));
        let announcer = SkillsAnnouncer::new(source);
        assert!(
            announcer.take().is_empty(),
            "empty catalog and no diagnostics must emit nothing"
        );
        Ok(())
    }

    #[test]
    fn harness_skills_announcer_absent_when_skills_disabled() {
        let (harness, _agent_id, _model_id) = build_harness_with_one_agent();
        assert!(
            harness.skills_announcer().is_none(),
            "harness without a skills source exposes no announcer"
        );
    }

    #[tokio::test]
    async fn spawner_spawns_runtime_agent_with_seeded_history_and_instruction()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::history::History;
        use crate::model::ModelStreamEvent;

        let (mut harness, _agent_id, _model_id) = build_harness_with_one_agent();
        harness.start().expect("start");
        let owner = harness.owner_id();

        // Build an inherited history prefix head to seed the new agent with.
        let mut prefix = History::new();
        prefix.append(Block::UserMessage {
            from: owner,
            content: "inherited context".to_string(),
            timestamp: SystemTime::now(),
        });
        let head = prefix.head().cloned();
        assert!(head.is_some(), "prefix head must be present");

        // The spawned agent reuses a model that captures its compiled input so
        // the test can confirm the seeded prefix and the instruction reached it.
        let model = Arc::new(StubArModel::with_events(vec![Ok(
            ModelStreamEvent::MessageComplete {
                usage: crate::model::Usage::default(),
            },
        )]));
        let captured = model.clone();

        let spawner = harness.spawner();
        let spawned_id = spawner
            .spawn(AgentSpec {
                system_prompt: prompt("spawned"),
                model,
                tools: harness.tools().clone(),
                history_prefix: head,
                instruction: Some(Block::UserMessage {
                    from: owner,
                    content: "do the thing".to_string(),
                    timestamp: SystemTime::now(),
                }),
                origin: None,
                forked_from: None,
            })
            .expect("spawn runtime agent");

        // The runtime is current-thread, so the spawned task cannot make
        // progress until this task awaits: subscribing now observes every event
        // the seeded instruction produces.
        let mut sub = harness.subscribe(spawned_id).expect("subscribe spawned");

        let mut saw_instruction = false;
        timeout(Duration::from_secs(2), async {
            while let Some(evt) = sub.next().await {
                if let AgentEvent::BlockComplete { block } = &evt
                    && let Block::UserMessage { content, .. } = block.block()
                    && content == "do the thing"
                {
                    saw_instruction = true;
                }
                if matches!(evt, AgentEvent::Idle) {
                    break;
                }
            }
        })
        .await
        .expect("spawned agent event wait timed out");
        assert!(
            saw_instruction,
            "expected the seeded instruction to reach the spawned agent"
        );

        // The compiled input carries both the inherited prefix block and the
        // instruction, in order after the system turn.
        let input = captured.last_input().expect("spawned model called");
        let user_texts: Vec<String> = input
            .turns
            .iter()
            .filter(|t| t.role == crate::model::Role::User)
            .filter_map(|t| match t.content.first() {
                Some(crate::model::ContentPart::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            user_texts,
            vec!["inherited context".to_string(), "do the thing".to_string()],
        );

        // shutdown + join must collect the runtime-spawned agent too.
        harness.shutdown();
        timeout(Duration::from_secs(5), harness.join())
            .await
            .expect("join timed out")
            .expect("clean shutdown");
        Ok(())
    }

    #[tokio::test]
    async fn spawner_emits_agent_forked_event_on_fork() -> Result<(), Box<dyn std::error::Error>> {
        use crate::history::{CheckpointHandle, NodeId};

        let (mut harness, origin_id, _model_id) = build_harness_with_one_agent();
        harness.start().expect("start");

        // Subscribe before spawning so the broadcast event is observed.
        let mut forks = harness.bus().subscribe_forks();

        let forked_from = CheckpointHandle::from_node_id(NodeId::new());
        let model = Arc::new(StubArModel::empty());
        let spawned = harness
            .spawner()
            .spawn(AgentSpec {
                system_prompt: prompt("forked"),
                model,
                tools: harness.tools().clone(),
                history_prefix: None,
                instruction: None,
                origin: Some(origin_id),
                forked_from: Some(forked_from),
            })
            .expect("spawn fork");

        let event = timeout(Duration::from_secs(2), forks.next())
            .await
            .expect("fork event timed out")
            .expect("fork stream closed");
        assert_eq!(event.agent_id, spawned, "event names the new peer");
        assert_eq!(event.origin, origin_id, "event names the originating agent");
        assert_eq!(
            event.handle, forked_from,
            "event carries the forked-from handle"
        );

        harness.shutdown();
        timeout(Duration::from_secs(5), harness.join())
            .await
            .expect("join timed out")
            .expect("clean shutdown");
        Ok(())
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

    /// Context-aware tool used by `context_aware_tool_reads_history_head_and_self_stops`.
    /// On invocation it records the calling Agent's live History (the seeded
    /// user messages found by walking the head's parent chain) and triggers its
    /// own self-stop via the [`ToolCallContext`] child token.
    struct HistoryStopTool {
        schema: serde_json::Value,
        captured_user_messages: Arc<Mutex<Vec<String>>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl HistoryStopTool {
        fn new(
            captured_user_messages: Arc<Mutex<Vec<String>>>,
            calls: Arc<std::sync::atomic::AtomicUsize>,
        ) -> Self {
            Self {
                schema: json!({ "type": "object" }),
                captured_user_messages,
                calls,
            }
        }
    }

    #[jsonrpsee::core::async_trait]
    impl Tool for HistoryStopTool {
        fn name(&self) -> &str {
            "history_stop"
        }

        fn description(&self) -> &str {
            "Records the calling agent's history and stops it."
        }

        fn input_schema(&self) -> &serde_json::Value {
            &self.schema
        }

        async fn call(
            &self,
            _input: serde_json::Value,
        ) -> Result<serde_json::Value, crate::tool::ToolError> {
            Ok(json!({ "stopped": false }))
        }

        async fn call_with_context(
            &self,
            _input: serde_json::Value,
            ctx: &crate::tool::ToolCallContext,
        ) -> Result<serde_json::Value, crate::tool::ToolError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

            let mut contents = Vec::new();
            let mut cursor = ctx.history_head().cloned();
            while let Some(node) = cursor {
                if let Block::UserMessage { content, .. } = node.block() {
                    contents.push(content.clone());
                }
                cursor = node.parent().cloned();
            }
            contents.reverse();
            *self.captured_user_messages.lock().expect("captured lock") = contents;

            ctx.stop();
            Ok(json!({ "stopped": true }))
        }
    }

    #[tokio::test]
    async fn context_aware_tool_reads_history_head_and_self_stops()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::model::{ModelStreamEvent, Usage};

        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls = Arc::new(AtomicUsize::new(0));

        // Call 1 emits a tool use for history_stop; call 2 (the re-entry after
        // the tool result) yields nothing, so the inner loop breaks and the
        // agent returns to awaiting inbound, where it observes its self-stop.
        let model = Arc::new(StubArModel::with_call_scripts(vec![
            vec![
                Ok(ModelStreamEvent::ToolUseComplete {
                    id: "use_1".to_string(),
                    name: "history_stop".to_string(),
                    input: json!({}),
                }),
                Ok(ModelStreamEvent::MessageComplete {
                    usage: Usage::default(),
                }),
            ],
            vec![Ok(ModelStreamEvent::MessageComplete {
                usage: Usage::default(),
            })],
        ]));

        let model_id = ModelId::new("stub");
        let agent_id = AgentId::new();
        let mut harness = HarnessBuilder::new()
            .add_model(model_id.clone(), model)
            .add_tool(Arc::new(HistoryStopTool::new(
                captured.clone(),
                calls.clone(),
            )))
            .expect("add tool")
            .add_agent(PendingAgent {
                id: agent_id,
                system_prompt: prompt("test"),
                model_id,
            })
            .build()
            .expect("build harness");
        harness.start().expect("start");

        let mut sub = harness.subscribe(agent_id).expect("subscribe");
        let owner = harness.owner_id();
        harness
            .send_to_agent(agent_id, owner, "remember me".to_string())
            .expect("send seed message");

        // The tool runs while draining to Idle: it reads history and self-stops.
        timeout(Duration::from_secs(2), async {
            while let Some(evt) = sub.next().await {
                if matches!(evt, AgentEvent::Idle) {
                    break;
                }
            }
        })
        .await
        .expect("idle wait timed out");

        assert_eq!(
            *captured.lock().expect("captured lock"),
            vec!["remember me".to_string()],
            "tool must read the calling agent's live History head chain",
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // The self-stop dropped the agent's inbound consumer, so a post-stop
        // send is refused: the agent no longer processes inbound.
        let post_stop = harness.send_to_agent(agent_id, owner, "ignored".to_string());
        assert!(
            post_stop.is_err(),
            "self-stopped agent must no longer accept inbound, got {post_stop:?}"
        );

        // join WITHOUT shutdown confirms termination: the agent's inbound is
        // never closed and shutdown is never called, so join can only return
        // because the self-stop already ended the task; a live agent would hang.
        timeout(Duration::from_secs(5), harness.join())
            .await
            .expect("join timed out — agent did not self-stop")
            .expect("clean self-stop");
        Ok(())
    }

    #[tokio::test]
    async fn nursery_stop_terminates_one_agent_and_sibling_keeps_running()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::model::{ModelStreamEvent, Usage};

        let model_a_id = ModelId::new("stub-a");
        let model_b_id = ModelId::new("stub-b");
        let agent_a = AgentId::new();
        let agent_b = AgentId::new();

        // Agent A idles on inbound with no scripted response. Agent B answers
        // one message, proving it still processes input after A is stopped.
        let model_a = Arc::new(StubArModel::empty());
        let model_b = Arc::new(StubArModel::with_events(vec![
            Ok(ModelStreamEvent::ContentComplete {
                text: "alive".to_string(),
            }),
            Ok(ModelStreamEvent::MessageComplete {
                usage: Usage::default(),
            }),
        ]));

        let mut harness = HarnessBuilder::new()
            .add_model(model_a_id.clone(), model_a)
            .add_model(model_b_id.clone(), model_b)
            .add_agent(PendingAgent {
                id: agent_a,
                system_prompt: prompt("a"),
                model_id: model_a_id,
            })
            .add_agent(PendingAgent {
                id: agent_b,
                system_prompt: prompt("b"),
                model_id: model_b_id,
            })
            .build()
            .expect("build harness");
        harness.start().expect("start");

        // Stop only agent A. It is waiting on inbound; its child-token
        // cancellation ends the task on the next poll.
        harness.nursery.stop(agent_a).expect("stop agent a");

        // Agent B still runs: it processes a message and reaches Idle.
        let mut sub_b = harness.subscribe(agent_b).expect("subscribe b");
        let owner = harness.owner_id();
        harness
            .send_to_agent(agent_b, owner, "ping".to_string())
            .expect("send b");

        let mut saw_alive = false;
        timeout(Duration::from_secs(2), async {
            while let Some(evt) = sub_b.next().await {
                if let AgentEvent::BlockComplete { block } = &evt
                    && let Block::AgentMessage { content, .. } = block.block()
                    && content == "alive"
                {
                    saw_alive = true;
                }
                if matches!(evt, AgentEvent::Idle) {
                    break;
                }
            }
        })
        .await
        .expect("agent b idle wait timed out");
        assert!(saw_alive, "sibling agent B should keep running and answer");

        // Close B's inbound so its loop exits. A's inbound is never closed and
        // shutdown is never called, so join can only return because A was
        // terminated by Nursery::stop; a live A would hang join.
        harness.bus().close_inbound(agent_b);
        timeout(Duration::from_secs(5), harness.join())
            .await
            .expect("join timed out — stopped agent kept the harness alive")
            .expect("clean join");
        Ok(())
    }
}
