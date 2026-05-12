//! MessageBus trait and in-memory implementation.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::{broadcast, mpsc};

use crate::history::{AgentId, Block, HistoryNode};
use crate::model::Usage;

#[derive(Clone, Debug)]
pub enum AgentEvent {
    TokenDelta { text: String },
    BlockComplete { block: std::sync::Arc<HistoryNode> },
    RunComplete { usage: Usage },
}

#[derive(Debug)]
pub enum Error {
    UnknownAgent(AgentId),
    Closed,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnknownAgent(id) => write!(f, "unknown agent: {id}"),
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
    fn register(
        &self,
        agent_id: AgentId,
    ) -> Result<(broadcast::Sender<AgentEvent>, mpsc::Receiver<Block>), Error>;

    fn outbound(&self, agent_id: AgentId) -> Result<broadcast::Sender<AgentEvent>, Error>;

    fn subscribe_outbound(
        &self,
        agent_id: AgentId,
    ) -> Result<broadcast::Receiver<AgentEvent>, Error>;

    fn send_inbound(&self, agent_id: AgentId, block: Block) -> Result<(), Error>;

    fn route(&self, from: AgentId, to: AgentId) -> Result<(), Error>;
}

// Broadcast channel is lossy by design; slow subscribers drop missed events.
const OUTBOUND_CAPACITY: usize = 1024;
const INBOUND_CAPACITY: usize = 64;

struct AgentEntry {
    outbound: broadcast::Sender<AgentEvent>,
    inbound: mpsc::Sender<Block>,
}

impl Clone for AgentEntry {
    fn clone(&self) -> Self {
        Self {
            outbound: self.outbound.clone(),
            inbound: self.inbound.clone(),
        }
    }
}

pub struct InMemoryMessageBus {
    entries: Mutex<HashMap<AgentId, AgentEntry>>,
}

impl InMemoryMessageBus {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn get_entry(&self, agent_id: AgentId) -> Result<AgentEntry, Error> {
        let map = self.entries.lock().map_err(|_| Error::Closed)?;
        map.get(&agent_id)
            .cloned()
            .ok_or(Error::UnknownAgent(agent_id))
    }
}

impl Default for InMemoryMessageBus {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageBus for InMemoryMessageBus {
    // Double-register silently overwrites the prior entry; callers are expected to register once.
    fn register(
        &self,
        agent_id: AgentId,
    ) -> Result<(broadcast::Sender<AgentEvent>, mpsc::Receiver<Block>), Error> {
        let (outbound_tx, _outbound_rx) = broadcast::channel(OUTBOUND_CAPACITY);
        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CAPACITY);
        let entry = AgentEntry {
            outbound: outbound_tx.clone(),
            inbound: inbound_tx,
        };
        let mut map = self.entries.lock().map_err(|_| Error::Closed)?;
        map.insert(agent_id, entry);
        Ok((outbound_tx, inbound_rx))
    }

    fn outbound(&self, agent_id: AgentId) -> Result<broadcast::Sender<AgentEvent>, Error> {
        let entry = self.get_entry(agent_id)?;
        Ok(entry.outbound)
    }

    fn subscribe_outbound(
        &self,
        agent_id: AgentId,
    ) -> Result<broadcast::Receiver<AgentEvent>, Error> {
        let entry = self.get_entry(agent_id)?;
        Ok(entry.outbound.subscribe())
    }

    // Uses try_send so the trait method stays sync; channel-full or closed maps to Error::Closed.
    fn send_inbound(&self, agent_id: AgentId, block: Block) -> Result<(), Error> {
        let entry = self.get_entry(agent_id)?;
        entry.inbound.try_send(block).map_err(|_| Error::Closed)
    }

    fn route(&self, from: AgentId, to: AgentId) -> Result<(), Error> {
        let from_tx = self.outbound(from)?;
        let to_inbound = {
            let map = self.entries.lock().map_err(|_| Error::Closed)?;
            let entry = map.get(&to).ok_or(Error::UnknownAgent(to))?;
            entry.inbound.clone()
        };
        let mut subscriber = from_tx.subscribe();
        tokio::spawn(async move {
            while let Ok(evt) = subscriber.recv().await {
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
    async fn register_then_send_inbound_delivers_block() {
        let bus = InMemoryMessageBus::new();
        let agent_id = AgentId::new();
        let (_tx, mut rx) = bus.register(agent_id).expect("register");

        bus.send_inbound(agent_id, user_block("hello"))
            .expect("send_inbound");

        let block = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("recv timed out")
            .expect("channel closed");

        match block {
            Block::UserMessage { content, .. } => assert_eq!(content, "hello"),
            _ => panic!("expected UserMessage"),
        }
    }

    #[tokio::test]
    async fn subscribe_outbound_sees_published_events() {
        let bus = InMemoryMessageBus::new();
        let agent_id = AgentId::new();
        let (_tx, _rx) = bus.register(agent_id).expect("register");

        let mut subscriber = bus.subscribe_outbound(agent_id).expect("subscribe");
        let outbound = bus.outbound(agent_id).expect("outbound");

        outbound
            .send(AgentEvent::TokenDelta {
                text: "hi".to_string(),
            })
            .expect("send event");

        let event = timeout(Duration::from_secs(1), subscriber.recv())
            .await
            .expect("recv timed out")
            .expect("broadcast closed");

        match event {
            AgentEvent::TokenDelta { text } => assert_eq!(text, "hi"),
            _ => panic!("expected TokenDelta"),
        }
    }

    #[tokio::test]
    async fn route_forwards_block_complete_with_agent_message() {
        let bus = InMemoryMessageBus::new();
        let a_id = AgentId::new();
        let b_id = AgentId::new();
        let (_a_tx, _a_rx) = bus.register(a_id).expect("register a");
        let (_b_tx, mut b_rx) = bus.register(b_id).expect("register b");

        bus.route(a_id, b_id).expect("route");

        let mut history = History::new();
        let node = history.append(agent_block(a_id, "from-a"));

        let outbound = bus.outbound(a_id).expect("outbound");
        outbound
            .send(AgentEvent::BlockComplete {
                block: Arc::clone(&node),
            })
            .expect("send block complete");

        let received = timeout(Duration::from_secs(1), b_rx.recv())
            .await
            .expect("recv timed out")
            .expect("inbound closed");

        match received {
            Block::AgentMessage { from, content, .. } => {
                assert_eq!(from, a_id);
                assert_eq!(content, "from-a");
            }
            _ => panic!("expected AgentMessage"),
        }
    }

    #[tokio::test]
    async fn route_ignores_non_agent_message_blocks() {
        let bus = InMemoryMessageBus::new();
        let a_id = AgentId::new();
        let b_id = AgentId::new();
        let (_a_tx, _a_rx) = bus.register(a_id).expect("register a");
        let (_b_tx, mut b_rx) = bus.register(b_id).expect("register b");

        bus.route(a_id, b_id).expect("route");

        let mut history = History::new();
        let node = history.append(user_block("should-not-route"));

        let outbound = bus.outbound(a_id).expect("outbound");
        outbound
            .send(AgentEvent::BlockComplete {
                block: Arc::clone(&node),
            })
            .expect("send block complete");

        let result = timeout(Duration::from_millis(200), b_rx.recv()).await;
        assert!(result.is_err(), "expected timeout, got {:?}", result);
    }

    #[tokio::test]
    async fn unknown_agent_returns_unknown_agent_error() {
        let bus = InMemoryMessageBus::new();
        let unknown = AgentId::new();
        let err = bus
            .send_inbound(unknown, user_block("x"))
            .expect_err("expected error");
        match err {
            Error::UnknownAgent(id) => assert_eq!(id, unknown),
            other => panic!("expected UnknownAgent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_outbound_on_unknown_agent_errors() {
        let bus = InMemoryMessageBus::new();
        let unknown = AgentId::new();
        let err = bus.subscribe_outbound(unknown).expect_err("expected error");
        match err {
            Error::UnknownAgent(_) => {}
            other => panic!("expected UnknownAgent, got {other:?}"),
        }
    }

    #[test]
    fn bus_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InMemoryMessageBus>();
    }
}
