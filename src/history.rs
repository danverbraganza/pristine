//! Persistent history of immutable Block events.

use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(Uuid);

impl NodeId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AgentId(Uuid);

impl AgentId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UserId(Uuid);

impl UserId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for UserId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug)]
pub enum Block {
    UserMessage {
        from: UserId,
        content: String,
        timestamp: SystemTime,
    },
    ReasoningTrace {
        content: String,
        timestamp: SystemTime,
    },
    ToolCall {
        name: String,
        arguments: serde_json::Value,
        timestamp: SystemTime,
    },
    ToolResult {
        name: String,
        result: serde_json::Value,
        timestamp: SystemTime,
    },
    AgentMessage {
        from: AgentId,
        content: String,
        timestamp: SystemTime,
    },
}

#[derive(Debug)]
pub struct HistoryNode {
    id: NodeId,
    block: Block,
    parent: Option<Arc<HistoryNode>>,
}

impl HistoryNode {
    pub(crate) fn new(id: NodeId, block: Block, parent: Option<Arc<HistoryNode>>) -> Self {
        Self { id, block, parent }
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn block(&self) -> &Block {
        &self.block
    }

    pub fn parent(&self) -> Option<&Arc<HistoryNode>> {
        self.parent.as_ref()
    }
}

pub struct History {
    head: Option<Arc<HistoryNode>>,
}

impl History {
    pub fn new() -> Self {
        Self { head: None }
    }

    pub fn append(&mut self, block: Block) -> Arc<HistoryNode> {
        let node = Arc::new(HistoryNode::new(NodeId::new(), block, self.head.clone()));
        self.head = Some(node.clone());
        node
    }

    pub fn head(&self) -> Option<&Arc<HistoryNode>> {
        self.head.as_ref()
    }

    pub fn linearize(&self) -> Vec<Block> {
        let mut out = Vec::new();
        let mut cursor = self.head.as_ref().cloned();
        while let Some(node) = cursor {
            out.push(node.block().clone());
            cursor = node.parent().cloned();
        }
        out.reverse();
        out
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_message(content: &str) -> Block {
        Block::UserMessage {
            from: UserId::new(),
            content: content.to_string(),
            timestamp: SystemTime::now(),
        }
    }

    #[test]
    fn append_advances_head_and_links_parent() {
        let mut history = History::new();
        let first = history.append(user_message("first"));
        let _second = history.append(user_message("second"));

        let head = history.head().expect("head should be set after append");
        let parent = head.parent().expect("second node should have a parent");
        assert!(Arc::ptr_eq(parent, &first));
        assert_eq!(parent.id(), first.id());
    }

    #[test]
    fn linearize_returns_root_first() {
        let mut history = History::new();
        history.append(user_message("one"));
        history.append(user_message("two"));
        history.append(user_message("three"));

        let blocks = history.linearize();
        let contents: Vec<&str> = blocks
            .iter()
            .map(|block| match block {
                Block::UserMessage { content, .. } => content.as_str(),
                _ => panic!("expected UserMessage variant"),
            })
            .collect();
        assert_eq!(contents, vec!["one", "two", "three"]);
    }

    #[test]
    fn fork_shares_prefix_via_arc() {
        let mut h1 = History::new();
        h1.append(user_message("shared-one"));
        let shared_head = h1.append(user_message("shared-two")).clone();

        let mut h2 = History {
            head: Some(shared_head.clone()),
        };

        let h1_tail = h1.append(user_message("h1-tail"));
        let h2_tail = h2.append(user_message("h2-tail"));

        let h1_blocks = h1.linearize();
        let h2_blocks = h2.linearize();

        let extract = |block: &Block| match block {
            Block::UserMessage { content, .. } => content.clone(),
            _ => panic!("expected UserMessage variant"),
        };

        assert_eq!(extract(&h1_blocks[0]), "shared-one");
        assert_eq!(extract(&h2_blocks[0]), "shared-one");
        assert_eq!(extract(&h1_blocks[1]), "shared-two");
        assert_eq!(extract(&h2_blocks[1]), "shared-two");
        assert_ne!(extract(&h1_blocks[2]), extract(&h2_blocks[2]));
        assert_eq!(extract(&h1_blocks[2]), "h1-tail");
        assert_eq!(extract(&h2_blocks[2]), "h2-tail");

        assert!(Arc::strong_count(&shared_head) >= 2);

        // Silence unused-binding warnings while keeping the tails pinned so the
        // strong_count assertion above reflects the intended sharing.
        let _ = (&h1_tail, &h2_tail);
    }
}
