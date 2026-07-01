//! Persistent history of immutable Block events.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(Uuid);

impl NodeId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// The reserved genesis identifier shared by every `History`.
    ///
    /// It names the synthetic, content-free root that precedes every real
    /// block; its checkpoint handle denotes the empty prefix.
    pub fn nil() -> Self {
        Self(Uuid::nil())
    }

    pub fn is_nil(&self) -> bool {
        self.0.is_nil()
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

/// Prefix marking the model-facing string form of a [`CheckpointHandle`].
const CHECKPOINT_HANDLE_PREFIX: &str = "ckpt-";

/// A stable, model-safe reference to a point in a `History`.
///
/// A handle is derived from a node's immutable [`NodeId`]. The genesis handle
/// (`NodeId::nil()`) is always available and denotes the empty prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CheckpointHandle(NodeId);

impl CheckpointHandle {
    /// The always-available handle denoting the empty prefix.
    pub fn genesis() -> Self {
        Self(NodeId::nil())
    }

    pub fn from_node_id(id: NodeId) -> Self {
        Self(id)
    }

    pub fn node_id(&self) -> NodeId {
        self.0
    }

    /// Whether this handle names the genesis (empty-prefix) checkpoint.
    pub fn is_genesis(&self) -> bool {
        self.0.is_nil()
    }
}

impl fmt::Display for CheckpointHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{CHECKPOINT_HANDLE_PREFIX}{}", self.0)
    }
}

impl FromStr for CheckpointHandle {
    type Err = HandleError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let body = s
            .strip_prefix(CHECKPOINT_HANDLE_PREFIX)
            .ok_or_else(|| HandleError::Malformed(s.to_string()))?;
        let uuid = Uuid::parse_str(body).map_err(|_| HandleError::Malformed(s.to_string()))?;
        Ok(Self(NodeId(uuid)))
    }
}

/// Failure modes when parsing or resolving a [`CheckpointHandle`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandleError {
    /// The string form was not a well-formed checkpoint handle.
    Malformed(String),
    /// The handle named a node that does not exist in this `History`.
    Unknown(CheckpointHandle),
}

impl fmt::Display for HandleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HandleError::Malformed(input) => write!(f, "malformed checkpoint handle: {input}"),
            HandleError::Unknown(handle) => write!(f, "unknown checkpoint handle: {handle}"),
        }
    }
}

impl std::error::Error for HandleError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
        id: String,
        name: String,
        arguments: serde_json::Value,
        timestamp: SystemTime,
    },
    ToolResult {
        tool_use_id: String,
        name: String,
        result: serde_json::Value,
        is_error: bool,
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

    /// The stable checkpoint handle naming this node.
    pub fn checkpoint_handle(&self) -> CheckpointHandle {
        CheckpointHandle::from_node_id(self.id)
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

    /// Construct a `History` seeded from an inherited prefix head.
    ///
    /// `Some(head)` starts the log at the given node, sharing the entire parent
    /// chain via `Arc` (the same prefix sharing `fork_shares_prefix_via_arc`
    /// demonstrates). `None` yields an empty log, identical to [`new`](Self::new).
    pub fn from_prefix(head: Option<Arc<HistoryNode>>) -> Self {
        Self { head }
    }

    pub fn append(&mut self, block: Block) -> Arc<HistoryNode> {
        let node = Arc::new(HistoryNode::new(NodeId::new(), block, self.head.clone()));
        self.head = Some(node.clone());
        node
    }

    pub fn head(&self) -> Option<&Arc<HistoryNode>> {
        self.head.as_ref()
    }

    /// Resolve a checkpoint handle against this `History`.
    ///
    /// The genesis handle (`NodeId::nil()`) resolves to `None` — the empty
    /// prefix. A handle naming a real node resolves to that node, whose parent
    /// chain is the history prefix ending at and including it. An unknown
    /// handle yields [`HandleError::Unknown`].
    pub fn resolve(
        &self,
        handle: &CheckpointHandle,
    ) -> Result<Option<Arc<HistoryNode>>, HandleError> {
        if handle.is_genesis() {
            return Ok(None);
        }
        let mut cursor = self.head.clone();
        while let Some(node) = cursor {
            if node.id() == handle.node_id() {
                return Ok(Some(node));
            }
            cursor = node.parent().cloned();
        }
        Err(HandleError::Unknown(*handle))
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

    /// Linearize in root-first order, pairing each block with the checkpoint
    /// handle of the node that carries it.
    ///
    /// This mirrors [`linearize`](Self::linearize) but retains each node's
    /// [`CheckpointHandle`] so compile-time consumers can render a handle
    /// against a block without a second traversal. `NodeId` is otherwise
    /// dropped by `linearize`.
    pub fn linearize_with_handles(&self) -> Vec<(CheckpointHandle, Block)> {
        let mut out = Vec::new();
        let mut cursor = self.head.as_ref().cloned();
        while let Some(node) = cursor {
            out.push((node.checkpoint_handle(), node.block().clone()));
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
    fn linearize_returns_root_first() -> Result<(), Box<dyn std::error::Error>> {
        let mut history = History::new();
        history.append(user_message("one"));
        history.append(user_message("two"));
        history.append(user_message("three"));

        let blocks = history.linearize();
        let contents: Vec<&str> = blocks
            .iter()
            .map(|block| match block {
                Block::UserMessage { content, .. } => Ok(content.as_str()),
                _ => Err("expected UserMessage variant"),
            })
            .collect::<Result<_, _>>()?;
        assert_eq!(contents, vec!["one", "two", "three"]);
        Ok(())
    }

    #[test]
    fn linearize_with_handles_pairs_each_block_with_its_node_handle()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut history = History::new();
        let first = history.append(user_message("one"));
        let second = history.append(user_message("two"));

        let paired = history.linearize_with_handles();
        assert_eq!(paired.len(), 2);
        assert_eq!(paired[0].0, first.checkpoint_handle());
        assert_eq!(paired[1].0, second.checkpoint_handle());
        match (&paired[0].1, &paired[1].1) {
            (Block::UserMessage { content: a, .. }, Block::UserMessage { content: b, .. }) => {
                assert_eq!(a, "one");
                assert_eq!(b, "two");
            }
            other => return Err(format!("expected UserMessage blocks, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn linearize_is_transparent_to_genesis() -> Result<(), Box<dyn std::error::Error>> {
        let mut history = History::new();
        history.append(user_message("one"));
        history.append(user_message("two"));

        let blocks = history.linearize();
        assert_eq!(blocks.len(), 2);

        let mut cursor = history.head().cloned();
        while let Some(node) = cursor {
            assert!(
                !node.id().is_nil(),
                "genesis must never appear in the chain"
            );
            cursor = node.parent().cloned();
        }

        assert!(history.resolve(&CheckpointHandle::genesis())?.is_none());
        Ok(())
    }

    #[test]
    fn genesis_handle_resolves_to_empty_prefix() -> Result<(), Box<dyn std::error::Error>> {
        let mut history = History::new();
        history.append(user_message("only"));

        assert!(CheckpointHandle::genesis().is_genesis());
        assert!(history.resolve(&CheckpointHandle::genesis())?.is_none());
        Ok(())
    }

    #[test]
    fn node_handle_round_trips_and_resolves() -> Result<(), Box<dyn std::error::Error>> {
        let mut history = History::new();
        history.append(user_message("first"));
        let target = history.append(user_message("second"));
        history.append(user_message("third"));

        let handle = target.checkpoint_handle();
        let rendered = handle.to_string();
        assert!(rendered.starts_with("ckpt-"));

        let parsed: CheckpointHandle = rendered.parse()?;
        assert_eq!(parsed, handle);

        let resolved = history
            .resolve(&parsed)?
            .ok_or("expected a node for a real handle")?;
        assert_eq!(resolved.id(), target.id());
        assert!(Arc::ptr_eq(&resolved, &target));
        Ok(())
    }

    #[test]
    fn unknown_handle_returns_typed_error() -> Result<(), Box<dyn std::error::Error>> {
        let mut history = History::new();
        history.append(user_message("only"));

        let orphan = CheckpointHandle::from_node_id(NodeId::new());
        match history.resolve(&orphan) {
            Err(HandleError::Unknown(handle)) => assert_eq!(handle, orphan),
            other => return Err(format!("expected Unknown handle error, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn malformed_handle_string_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        assert!(matches!(
            "not-a-handle".parse::<CheckpointHandle>(),
            Err(HandleError::Malformed(_))
        ));
        assert!(matches!(
            "ckpt-not-a-uuid".parse::<CheckpointHandle>(),
            Err(HandleError::Malformed(_))
        ));
        Ok(())
    }

    #[test]
    fn fork_shares_prefix_via_arc() -> Result<(), Box<dyn std::error::Error>> {
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

        let extract = |block: &Block| -> Result<String, Box<dyn std::error::Error>> {
            match block {
                Block::UserMessage { content, .. } => Ok(content.clone()),
                _ => Err("expected UserMessage variant".into()),
            }
        };

        assert_eq!(extract(&h1_blocks[0])?, "shared-one");
        assert_eq!(extract(&h2_blocks[0])?, "shared-one");
        assert_eq!(extract(&h1_blocks[1])?, "shared-two");
        assert_eq!(extract(&h2_blocks[1])?, "shared-two");
        assert_ne!(extract(&h1_blocks[2])?, extract(&h2_blocks[2])?);
        assert_eq!(extract(&h1_blocks[2])?, "h1-tail");
        assert_eq!(extract(&h2_blocks[2])?, "h2-tail");

        assert!(Arc::strong_count(&shared_head) >= 2);

        // Silence unused-binding warnings while keeping the tails pinned so the
        // strong_count assertion above reflects the intended sharing.
        let _ = (&h1_tail, &h2_tail);
        Ok(())
    }

    #[test]
    fn tool_call_round_trips_through_history() -> Result<(), Box<dyn std::error::Error>> {
        let mut history = History::new();
        history.append(Block::ToolCall {
            id: "use_001".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({"text": "hi"}),
            timestamp: SystemTime::now(),
        });

        let blocks = history.linearize();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::ToolCall {
                id,
                name,
                arguments,
                timestamp,
            } => {
                assert_eq!(id, "use_001");
                assert_eq!(name, "echo");
                assert_eq!(arguments, &serde_json::json!({"text": "hi"}));
                assert!(matches!(timestamp, SystemTime { .. }));
            }
            other => return Err(format!("expected ToolCall, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn tool_result_round_trips_through_history() -> Result<(), Box<dyn std::error::Error>> {
        let mut history = History::new();
        history.append(Block::ToolResult {
            tool_use_id: "use_002".to_string(),
            name: "echo".to_string(),
            result: serde_json::Value::Null,
            is_error: true,
            timestamp: SystemTime::now(),
        });

        let blocks = history.linearize();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::ToolResult {
                tool_use_id,
                name,
                result,
                is_error,
                timestamp,
            } => {
                assert_eq!(tool_use_id, "use_002");
                assert_eq!(name, "echo");
                assert_eq!(result, &serde_json::Value::Null);
                assert!(*is_error);
                assert!(matches!(timestamp, SystemTime { .. }));
            }
            other => return Err(format!("expected ToolResult, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn tool_call_and_result_share_correlation_id() -> Result<(), Box<dyn std::error::Error>> {
        let mut history = History::new();
        history.append(Block::ToolCall {
            id: "use_003".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({"text": "hello"}),
            timestamp: SystemTime::now(),
        });
        history.append(Block::ToolResult {
            tool_use_id: "use_003".to_string(),
            name: "echo".to_string(),
            result: serde_json::json!({"echoed": "hello"}),
            is_error: false,
            timestamp: SystemTime::now(),
        });

        let blocks = history.linearize();
        assert_eq!(blocks.len(), 2);
        let call_id = match &blocks[0] {
            Block::ToolCall { id, .. } => id.clone(),
            other => return Err(format!("expected ToolCall, got {other:?}").into()),
        };
        let result_id = match &blocks[1] {
            Block::ToolResult { tool_use_id, .. } => tool_use_id.clone(),
            other => return Err(format!("expected ToolResult, got {other:?}").into()),
        };
        assert_eq!(call_id, result_id);
        Ok(())
    }
}
