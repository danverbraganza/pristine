//! MessageBus trait and in-memory implementation.

use std::collections::HashMap;
use std::sync::Mutex;

use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::{BroadcastStream, ReceiverStream};

use crate::history::{AgentId, Block, CheckpointHandle, HistoryNode};
use crate::model::Usage;

#[derive(Clone, Debug)]
pub enum AgentEvent {
    TokenDelta {
        text: String,
    },
    /// A partial chunk of the active Model call's streaming reasoning output.
    /// Best-effort delivery; slow consumers may miss intermediate deltas.
    ReasoningDelta {
        text: String,
    },
    BlockComplete {
        block: std::sync::Arc<HistoryNode>,
    },
    RunComplete {
        usage: Usage,
    },
    Error {
        message: String,
    },
    /// Emitted once after the agent finishes processing one inbound Block.
    /// Subscribers use this as the "agent ready for next input" signal.
    Idle,
}

/// A control message delivered to a running Agent out-of-band from its `Block`
/// inbound stream. Control messages drive the Agent without contributing a
/// `Block` to History or triggering a model turn for the request itself; they
/// layer alongside the append-only `Block` vocabulary rather than extending it.
#[derive(Clone, Debug)]
pub enum Control {
    /// Ask the Agent to fork itself using its own live runtime context. The
    /// Agent resolves this against the same inputs it assembles at tool
    /// dispatch and spawns a peer via the shared fork logic.
    Fork(ForkRequest),
}

/// Parameters for an inbound fork-control request, mirroring the Fork tool's
/// parameters so both paths reuse the same fork logic.
#[derive(Clone, Debug)]
pub struct ForkRequest {
    /// The immediate next instruction seeded as the fork's first inbound
    /// message.
    pub instruction: String,
    /// Optional checkpoint handle bounding the inherited history prefix. Omitted
    /// inherits the full prior context; the genesis handle inherits none.
    pub handle: Option<String>,
    /// Optional subset of the parent's tools to narrow the fork to. Omitted
    /// inherits all of the parent's tools.
    pub tools: Option<Vec<String>>,
}

/// A fork-spawned event emitted on the bus whenever a peer Agent is forked,
/// covering both tool-initiated and control-path forks. This is the internal
/// signal a transport surfaces as an `agent_forked` notification.
#[derive(Clone, Copy, Debug)]
pub struct AgentForked {
    /// The freshly spawned peer Agent's id.
    pub agent_id: AgentId,
    /// The originating Agent that forked.
    pub origin: AgentId,
    /// The checkpoint handle the fork inherited history up to and including.
    pub handle: CheckpointHandle,
}

#[derive(Debug)]
pub enum Error {
    UnknownAgent(AgentId),
    AlreadyRegistered(AgentId),
    Closed,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnknownAgent(id) => write!(f, "unknown agent: {id}"),
            Error::AlreadyRegistered(id) => write!(f, "agent already registered: {id}"),
            Error::Closed => write!(f, "channel closed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

pub trait MessageBus: Send + Sync {
    /// Register an Agent and obtain its single-consumer inbound stream.
    /// A second `register` for the same `AgentId` returns `Error::AlreadyRegistered`.
    fn register(&self, agent_id: AgentId) -> Result<BoxStream<'static, Block>, Error>;

    /// Publish an event on a registered Agent's outbound stream.
    fn publish(&self, agent_id: AgentId, event: AgentEvent) -> Result<(), Error>;

    /// Subscribe to a registered Agent's outbound events. Multi-subscriber;
    /// each call returns a fresh stream observing events from the call site onward.
    fn subscribe(&self, agent_id: AgentId) -> Result<BoxStream<'static, AgentEvent>, Error>;

    /// Push a `Block` onto the named Agent's inbound stream.
    fn send_inbound(&self, agent_id: AgentId, block: Block) -> Result<(), Error>;

    /// Obtain the named Agent's out-of-band control stream. Control messages
    /// arrive here rather than on the `Block` inbound stream, so `Block`
    /// delivery is unaffected.
    fn control_stream(&self, agent_id: AgentId) -> Result<BoxStream<'static, Control>, Error>;

    /// Push a [`Control`] message onto the named Agent's control stream.
    fn send_control(&self, agent_id: AgentId, control: Control) -> Result<(), Error>;

    /// Broadcast an [`AgentForked`] event to every fork subscriber.
    fn publish_fork(&self, event: AgentForked) -> Result<(), Error>;

    /// Subscribe to [`AgentForked`] events across the bus. Multi-subscriber;
    /// each call returns a fresh stream observing events from the call site onward.
    fn subscribe_forks(&self) -> BoxStream<'static, AgentForked>;

    /// Route `AgentMessage` Blocks from `from`'s outbound stream to `to`'s inbound stream.
    /// Spawns an internal task. Other event variants are observed but not forwarded.
    fn route(&self, from: AgentId, to: AgentId) -> Result<(), Error>;
}

// Broadcast channel is lossy by design; slow subscribers drop missed events.
const OUTBOUND_CAPACITY: usize = 1024;
const INBOUND_CAPACITY: usize = 64;

struct AgentEntry {
    outbound: broadcast::Sender<AgentEvent>,
    inbound_sender: mpsc::Sender<Block>,
    control: broadcast::Sender<Control>,
}

pub struct InMemoryMessageBus {
    entries: Mutex<HashMap<AgentId, AgentEntry>>,
    forks: broadcast::Sender<AgentForked>,
}

impl InMemoryMessageBus {
    pub fn new() -> Self {
        let (forks, _forks_rx) = broadcast::channel(OUTBOUND_CAPACITY);
        Self {
            entries: Mutex::new(HashMap::new()),
            forks,
        }
    }

    fn outbound_sender(&self, agent_id: AgentId) -> Result<broadcast::Sender<AgentEvent>, Error> {
        let map = self.entries.lock().map_err(|_| Error::Closed)?;
        map.get(&agent_id)
            .map(|e| e.outbound.clone())
            .ok_or(Error::UnknownAgent(agent_id))
    }

    fn inbound_sender(&self, agent_id: AgentId) -> Result<mpsc::Sender<Block>, Error> {
        let map = self.entries.lock().map_err(|_| Error::Closed)?;
        map.get(&agent_id)
            .map(|e| e.inbound_sender.clone())
            .ok_or(Error::UnknownAgent(agent_id))
    }

    fn control_sender(&self, agent_id: AgentId) -> Result<broadcast::Sender<Control>, Error> {
        let map = self.entries.lock().map_err(|_| Error::Closed)?;
        map.get(&agent_id)
            .map(|e| e.control.clone())
            .ok_or(Error::UnknownAgent(agent_id))
    }

    // Test-only: drop the entry so the inbound mpsc sender closes and the
    // Agent's inbound stream yields None on its next poll.
    #[cfg(test)]
    pub fn close_inbound(&self, agent_id: AgentId) {
        if let Ok(mut map) = self.entries.lock() {
            map.remove(&agent_id);
        }
    }
}

impl Default for InMemoryMessageBus {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageBus for InMemoryMessageBus {
    fn register(&self, agent_id: AgentId) -> Result<BoxStream<'static, Block>, Error> {
        let mut map = self.entries.lock().map_err(|_| Error::Closed)?;
        if map.contains_key(&agent_id) {
            return Err(Error::AlreadyRegistered(agent_id));
        }
        let (outbound_tx, _outbound_rx) = broadcast::channel(OUTBOUND_CAPACITY);
        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CAPACITY);
        let (control_tx, _control_rx) = broadcast::channel(INBOUND_CAPACITY);
        map.insert(
            agent_id,
            AgentEntry {
                outbound: outbound_tx,
                inbound_sender: inbound_tx,
                control: control_tx,
            },
        );
        Ok(Box::pin(ReceiverStream::new(inbound_rx)))
    }

    fn publish(&self, agent_id: AgentId, event: AgentEvent) -> Result<(), Error> {
        let sender = self.outbound_sender(agent_id)?;
        // Broadcast send errors only when there are zero receivers; the bus is lossy by design.
        let _ = sender.send(event);
        Ok(())
    }

    fn subscribe(&self, agent_id: AgentId) -> Result<BoxStream<'static, AgentEvent>, Error> {
        let sender = self.outbound_sender(agent_id)?;
        let receiver = sender.subscribe();
        let stream = BroadcastStream::new(receiver).filter_map(|res| async move { res.ok() });
        Ok(Box::pin(stream))
    }

    fn send_inbound(&self, agent_id: AgentId, block: Block) -> Result<(), Error> {
        let sender = self.inbound_sender(agent_id)?;
        sender.try_send(block).map_err(|_| Error::Closed)
    }

    fn control_stream(&self, agent_id: AgentId) -> Result<BoxStream<'static, Control>, Error> {
        let sender = self.control_sender(agent_id)?;
        let stream =
            BroadcastStream::new(sender.subscribe()).filter_map(|res| async move { res.ok() });
        Ok(Box::pin(stream))
    }

    fn send_control(&self, agent_id: AgentId, control: Control) -> Result<(), Error> {
        let sender = self.control_sender(agent_id)?;
        // Broadcast send errors only when there are zero receivers; the Agent
        // subscribes at build time, and the channel is lossy by design.
        let _ = sender.send(control);
        Ok(())
    }

    fn publish_fork(&self, event: AgentForked) -> Result<(), Error> {
        // Lossy by design: a fork event is dropped only when no one is watching.
        let _ = self.forks.send(event);
        Ok(())
    }

    fn subscribe_forks(&self) -> BoxStream<'static, AgentForked> {
        let stream =
            BroadcastStream::new(self.forks.subscribe()).filter_map(|res| async move { res.ok() });
        Box::pin(stream)
    }

    fn route(&self, from: AgentId, to: AgentId) -> Result<(), Error> {
        let mut subscriber = self.subscribe(from)?;
        let to_inbound = self.inbound_sender(to)?;
        tokio::spawn(async move {
            while let Some(evt) = subscriber.next().await {
                if let AgentEvent::BlockComplete { block } = evt
                    && let Block::AgentMessage { .. } = block.block()
                {
                    let cloned = block.block().clone();
                    if to_inbound.send(cloned).await.is_err() {
                        break;
                    }
                }
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use tokio::time::timeout;

    use crate::history::{History, UserId};

    fn user_block(content: &str) -> Block {
        Block::UserMessage {
            from: UserId::new(),
            content: content.to_string(),
            timestamp: SystemTime::now(),
        }
    }

    fn agent_block(from: AgentId, content: &str) -> Block {
        Block::AgentMessage {
            from,
            content: content.to_string(),
            timestamp: SystemTime::now(),
        }
    }

    #[tokio::test]
    async fn register_then_send_inbound_delivers_block() -> Result<(), Box<dyn std::error::Error>> {
        let bus = InMemoryMessageBus::new();
        let agent_id = AgentId::new();
        let mut inbound = bus.register(agent_id).expect("register");

        bus.send_inbound(agent_id, user_block("hello"))
            .expect("send_inbound");

        let block = timeout(Duration::from_secs(1), inbound.next())
            .await
            .expect("recv timed out")
            .expect("stream closed");

        match block {
            Block::UserMessage { content, .. } => assert_eq!(content, "hello"),
            _ => return Err("expected UserMessage".into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn subscribe_sees_published_events() -> Result<(), Box<dyn std::error::Error>> {
        let bus = InMemoryMessageBus::new();
        let agent_id = AgentId::new();
        let _inbound = bus.register(agent_id).expect("register");

        let mut subscriber = bus.subscribe(agent_id).expect("subscribe");

        bus.publish(
            agent_id,
            AgentEvent::TokenDelta {
                text: "hi".to_string(),
            },
        )
        .expect("publish event");

        let event = timeout(Duration::from_secs(1), subscriber.next())
            .await
            .expect("recv timed out")
            .expect("stream closed");

        match event {
            AgentEvent::TokenDelta { text } => assert_eq!(text, "hi"),
            _ => return Err("expected TokenDelta".into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn route_forwards_block_complete_with_agent_message()
    -> Result<(), Box<dyn std::error::Error>> {
        let bus = InMemoryMessageBus::new();
        let a_id = AgentId::new();
        let b_id = AgentId::new();
        let _a_inbound = bus.register(a_id).expect("register a");
        let mut b_inbound = bus.register(b_id).expect("register b");

        bus.route(a_id, b_id).expect("route");

        let mut history = History::new();
        let node = history.append(agent_block(a_id, "from-a"));

        bus.publish(
            a_id,
            AgentEvent::BlockComplete {
                block: Arc::clone(&node),
            },
        )
        .expect("publish block complete");

        let received = timeout(Duration::from_secs(1), b_inbound.next())
            .await
            .expect("recv timed out")
            .expect("stream closed");

        match received {
            Block::AgentMessage { from, content, .. } => {
                assert_eq!(from, a_id);
                assert_eq!(content, "from-a");
            }
            _ => return Err("expected AgentMessage".into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn route_ignores_non_agent_message_blocks() {
        let bus = InMemoryMessageBus::new();
        let a_id = AgentId::new();
        let b_id = AgentId::new();
        let _a_inbound = bus.register(a_id).expect("register a");
        let mut b_inbound = bus.register(b_id).expect("register b");

        bus.route(a_id, b_id).expect("route");

        let mut history = History::new();
        let node = history.append(user_block("should-not-route"));

        bus.publish(
            a_id,
            AgentEvent::BlockComplete {
                block: Arc::clone(&node),
            },
        )
        .expect("publish block complete");

        let result = timeout(Duration::from_millis(200), b_inbound.next()).await;
        assert!(result.is_err(), "expected timeout, got {:?}", result);
    }

    #[tokio::test]
    async fn unknown_agent_returns_unknown_agent_error() -> Result<(), Box<dyn std::error::Error>> {
        let bus = InMemoryMessageBus::new();
        let unknown = AgentId::new();
        let err = bus
            .send_inbound(unknown, user_block("x"))
            .expect_err("expected error");
        match err {
            Error::UnknownAgent(id) => assert_eq!(id, unknown),
            other => return Err(format!("expected UnknownAgent, got {other:?}").into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn subscribe_on_unknown_agent_errors() -> Result<(), Box<dyn std::error::Error>> {
        let bus = InMemoryMessageBus::new();
        let unknown = AgentId::new();
        match bus.subscribe(unknown) {
            Err(Error::UnknownAgent(_)) => {}
            Err(other) => return Err(format!("expected UnknownAgent, got {other:?}").into()),
            Ok(_) => return Err("expected error, got Ok".into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn subscribe_sees_published_error_events() -> Result<(), Box<dyn std::error::Error>> {
        let bus = InMemoryMessageBus::new();
        let agent_id = AgentId::new();
        let _inbound = bus.register(agent_id).expect("register");

        let mut subscriber = bus.subscribe(agent_id).expect("subscribe");

        bus.publish(
            agent_id,
            AgentEvent::Error {
                message: "boom".to_string(),
            },
        )
        .expect("publish event");

        let event = timeout(Duration::from_secs(1), subscriber.next())
            .await
            .expect("recv timed out")
            .expect("stream closed");

        match event {
            AgentEvent::Error { message } => assert_eq!(message, "boom"),
            other => return Err(format!("expected Error, got {other:?}").into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn register_twice_errors() -> Result<(), Box<dyn std::error::Error>> {
        let bus = InMemoryMessageBus::new();
        let agent_id = AgentId::new();
        let _first = bus.register(agent_id).expect("first register");
        match bus.register(agent_id) {
            Err(Error::AlreadyRegistered(id)) => assert_eq!(id, agent_id),
            Err(other) => return Err(format!("expected AlreadyRegistered, got {other:?}").into()),
            Ok(_) => return Err("expected second register to fail".into()),
        }
        Ok(())
    }

    #[test]
    fn bus_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InMemoryMessageBus>();
    }
}
