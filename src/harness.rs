//! Harness lifecycle and registry.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use futures::stream::BoxStream;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentBuilder;
use crate::history::{AgentId, Block};
use crate::messagebus::{AgentEvent, InMemoryMessageBus, MessageBus};
use crate::model::{ARModel, ModelRole};
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
    pub system_prompt: String,
    pub model_id: ModelId,
}

#[derive(Debug)]
pub enum Error {
    UnknownModel(ModelId),
    UnknownAgent(AgentId),
    Lifecycle(String),
    Bus(crate::messagebus::Error),
    Join(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnknownModel(id) => write!(f, "unknown model: {id}"),
            Error::UnknownAgent(id) => write!(f, "unknown agent: {id}"),
            Error::Lifecycle(msg) => write!(f, "lifecycle error: {msg}"),
            Error::Bus(err) => write!(f, "message bus error: {err}"),
            Error::Join(msg) => write!(f, "task join error: {msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Bus(err) => Some(err),
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
}

pub struct HarnessBuilder {
    models: HashMap<ModelId, Arc<dyn ARModel>>,
    pending: Vec<PendingAgent>,
}

impl HarnessBuilder {
    pub fn new() -> Self {
        Self {
            models: HashMap::new(),
            pending: Vec::new(),
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

    // Test-only bus accessor for publishing AgentEvents directly without driving the agent loop.
    #[cfg(test)]
    pub fn bus(&self) -> &Arc<InMemoryMessageBus> {
        &self.bus
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use futures::StreamExt;
    use tokio::time::timeout;

    use crate::messagebus::AgentEvent;
    use crate::model::Error as ModelError;
    use crate::test_support::StubArModel;

    fn build_harness_with_one_agent() -> (Harness, AgentId, ModelId) {
        let model_id = ModelId::new("stub");
        let agent_id = AgentId::new();
        let harness = HarnessBuilder::new()
            .add_model(model_id.clone(), Arc::new(StubArModel::empty()))
            .add_agent(PendingAgent {
                id: agent_id,
                system_prompt: "test".to_string(),
                model_id: model_id.clone(),
            })
            .build()
            .expect("build harness");
        (harness, agent_id, model_id)
    }

    #[test]
    fn builder_rejects_agent_with_unknown_model() {
        let missing = ModelId::new("missing");
        let agent_id = AgentId::new();
        let result = HarnessBuilder::new()
            .add_agent(PendingAgent {
                id: agent_id,
                system_prompt: "test".to_string(),
                model_id: missing.clone(),
            })
            .build();
        match result {
            Err(Error::UnknownModel(id)) => assert_eq!(id, missing),
            Err(other) => panic!("expected UnknownModel, got {other:?}"),
            Ok(_) => panic!("builder should reject unknown model"),
        }
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
    async fn subscribe_returns_receiver_seeing_published_event() {
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
            other => panic!("expected TokenDelta, got {other:?}"),
        }

        harness.shutdown();
        timeout(Duration::from_secs(5), harness.join())
            .await
            .expect("join timed out")
            .expect("clean shutdown");
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
                system_prompt: "test".to_string(),
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
                system_prompt: "a".to_string(),
                model_id: model_id_a,
            })
            .add_agent(PendingAgent {
                id: agent_b,
                system_prompt: "b".to_string(),
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
}
