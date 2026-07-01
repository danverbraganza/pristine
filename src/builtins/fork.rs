//! Fork: spawns a peer Agent seeded from the calling agent's live runtime
//! state. The fork inherits the parent's system prompt, model, and tool set
//! (optionally narrowed), plus a history prefix bounded by an optional
//! checkpoint handle. Its constructor takes no behavioral arguments; every
//! input arrives at call time through the [`ToolCallContext`].

use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;

use serde_json::{Value, json};

use crate::harness::AgentSpec;
use crate::history::{AgentId, Block, CheckpointHandle, History, UserId};
use crate::model::ModelRole;
use crate::tool::{Tool, ToolCallContext, ToolError, ToolRegistry, execution_err};

#[derive(serde::Deserialize)]
struct ForkInput {
    instruction: String,
    handle: Option<String>,
    tools: Option<Vec<String>>,
}

/// Typed failure modes shared by the Fork tool and the agent control path.
///
/// Serializes as a `kind`-tagged JSON object, so the Fork tool routes it
/// through the [`execution_err`] carrier unchanged, and the control path can
/// render it into an error event.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ForkError {
    InvalidHandle { handle: String, reason: String },
    UnknownTool { name: String },
    NoModel,
    SpawnFailed { reason: String },
}

impl std::fmt::Display for ForkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForkError::InvalidHandle { handle, reason } => {
                write!(f, "invalid fork handle {handle}: {reason}")
            }
            ForkError::UnknownTool { name } => write!(f, "unknown tool: {name}"),
            ForkError::NoModel => write!(f, "no default model to fork"),
            ForkError::SpawnFailed { reason } => write!(f, "fork spawn failed: {reason}"),
        }
    }
}

impl std::error::Error for ForkError {}

/// Successful outcome of a fork: the spawned peer's id and the checkpoint handle
/// the fork inherited history up to and including.
pub(crate) struct ForkOutcome {
    pub agent_id: AgentId,
    pub handle: CheckpointHandle,
}

/// Core fork logic shared by the Fork tool and the agent control path.
///
/// Resolves the optional history handle against the caller's live head
/// (omitted inherits the full prefix, genesis inherits none, invalid surfaces
/// as [`ForkError::InvalidHandle`]), narrows the tool set to the requested
/// subset if any, constructs an [`AgentSpec`] inheriting the caller's system
/// prompt and default model, and spawns the peer. The spawn carries the
/// originating agent and the inherited handle so the spawner path can emit a
/// uniform agent-forked event.
pub(crate) fn fork_from_context(
    ctx: &ToolCallContext,
    instruction: String,
    handle: Option<String>,
    tools: Option<Vec<String>>,
) -> Result<ForkOutcome, ForkError> {
    let (history_prefix, fork_handle) = match &handle {
        None => {
            let head = ctx.history_head().cloned();
            let handle = head
                .as_ref()
                .map(|node| node.checkpoint_handle())
                .unwrap_or_else(CheckpointHandle::genesis);
            (head, handle)
        }
        Some(raw) => {
            let handle = CheckpointHandle::from_str(raw).map_err(|e| ForkError::InvalidHandle {
                handle: raw.clone(),
                reason: e.to_string(),
            })?;
            let history = History::from_prefix(ctx.history_head().cloned());
            let resolved = history
                .resolve(&handle)
                .map_err(|e| ForkError::InvalidHandle {
                    handle: raw.clone(),
                    reason: e.to_string(),
                })?;
            (resolved, handle)
        }
    };

    let tools = match &tools {
        None => ctx.tools().clone(),
        Some(names) => {
            let mut narrowed = ToolRegistry::new();
            for name in names {
                let tool = ctx
                    .tools()
                    .get(name)
                    .ok_or_else(|| ForkError::UnknownTool { name: name.clone() })?;
                narrowed
                    .register(tool)
                    .map_err(|e| ForkError::SpawnFailed {
                        reason: e.to_string(),
                    })?;
            }
            Arc::new(narrowed)
        }
    };

    let model = ctx
        .model(ModelRole::Default)
        .ok_or(ForkError::NoModel)?
        .clone();

    let instruction = Block::UserMessage {
        from: UserId::new(),
        content: instruction,
        timestamp: SystemTime::now(),
    };

    let spec = AgentSpec {
        origin: Some(ctx.agent_id()),
        forked_from: Some(fork_handle),
        system_prompt: ctx.system_prompt().clone(),
        model,
        tools,
        history_prefix,
        instruction: Some(instruction),
    };

    let agent_id = ctx
        .spawner()
        .spawn(spec)
        .map_err(|e| ForkError::SpawnFailed {
            reason: e.to_string(),
        })?;

    Ok(ForkOutcome {
        agent_id,
        handle: fork_handle,
    })
}

pub struct Fork {
    schema: Value,
}

impl Fork {
    pub fn new() -> Self {
        Self {
            schema: json!({
                "type": "object",
                "properties": {
                    "instruction": {"type": "string"},
                    "handle": {"type": "string"},
                    "tools": {
                        "type": "array",
                        "items": {"type": "string"}
                    }
                },
                "required": ["instruction"]
            }),
        }
    }
}

impl Default for Fork {
    fn default() -> Self {
        Self::new()
    }
}

#[jsonrpsee::core::async_trait]
impl Tool for Fork {
    fn name(&self) -> &str {
        "fork"
    }

    fn description(&self) -> &str {
        "Fork a new peer agent that inherits this agent's system prompt, model, \
         and tools. `instruction` seeds the fork's first inbound message. \
         Optional `handle` names a checkpoint boundary to inherit history up to \
         and including (omitted inherits the full context; the genesis handle \
         inherits none). Optional `tools` narrows the inherited tool set to the \
         named subset. Returns `{agent_id, handle}`."
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    async fn call(&self, _input: Value) -> Result<Value, ToolError> {
        Err(ToolError::InvalidInput(
            "fork requires the calling agent's context and cannot be called \
             without it"
                .to_string(),
        ))
    }

    async fn call_with_context(
        &self,
        input: Value,
        ctx: &ToolCallContext,
    ) -> Result<Value, ToolError> {
        let parsed: ForkInput = serde_json::from_value(input).map_err(|e| {
            ToolError::InvalidInput(format!(
                "fork requires string field 'instruction' and optional fields \
                 'handle' (string) and 'tools' (array of string): {e}"
            ))
        })?;

        let outcome = fork_from_context(ctx, parsed.instruction, parsed.handle, parsed.tools)
            .map_err(execution_err)?;

        Ok(json!({
            "agent_id": outcome.agent_id.to_string(),
            "handle": outcome.handle.to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use tokio_util::sync::CancellationToken;

    use crate::agent::SystemPrompt;
    use crate::history::{AgentId, History, HistoryNode};
    use crate::model::ARModel;
    use crate::test_support::{EchoTool, RecordingSpawner, StubArModel, execution_value};

    fn prompt() -> SystemPrompt {
        SystemPrompt {
            base: "parent prompt".to_string(),
            skills: None,
        }
    }

    fn models() -> HashMap<ModelRole, Arc<dyn ARModel>> {
        let mut map: HashMap<ModelRole, Arc<dyn ARModel>> = HashMap::new();
        map.insert(ModelRole::Default, Arc::new(StubArModel::empty()));
        map
    }

    fn user_message(content: &str) -> Block {
        Block::UserMessage {
            from: UserId::new(),
            content: content.to_string(),
            timestamp: SystemTime::now(),
        }
    }

    /// Assemble a context around `head` and `tools`, sharing `spawner`.
    fn context(
        head: Option<Arc<HistoryNode>>,
        tools: Arc<ToolRegistry>,
        spawner: Arc<RecordingSpawner>,
    ) -> ToolCallContext {
        ToolCallContext::new(
            AgentId::new(),
            head,
            prompt(),
            models(),
            tools,
            spawner,
            CancellationToken::new(),
        )
    }

    fn instruction_content(spec: &AgentSpec) -> Result<String, Box<dyn std::error::Error>> {
        match &spec.instruction {
            Some(Block::UserMessage { content, .. }) => Ok(content.clone()),
            other => {
                Err(format!("expected a seeded UserMessage instruction, got {other:?}").into())
            }
        }
    }

    #[tokio::test]
    async fn omitted_handle_inherits_full_prefix() -> Result<(), Box<dyn std::error::Error>> {
        let mut history = History::new();
        history.append(user_message("one"));
        let head = history.append(user_message("two"));

        let spawner = Arc::new(RecordingSpawner::new());
        let ctx = context(
            Some(head.clone()),
            Arc::new(ToolRegistry::new()),
            spawner.clone(),
        );
        let tool = Fork::new();

        let value = tool
            .call_with_context(json!({ "instruction": "go" }), &ctx)
            .await
            .expect("fork succeeds");

        let specs = spawner.specs();
        assert_eq!(specs.len(), 1, "one agent must be spawned");
        let prefix = specs[0]
            .history_prefix
            .as_ref()
            .ok_or("omitted handle inherits the full prefix")?;
        assert_eq!(prefix.id(), head.id(), "prefix head is the calling head");
        assert_eq!(instruction_content(&specs[0])?, "go");
        assert_eq!(
            value["handle"].as_str().ok_or("handle is a string")?,
            head.checkpoint_handle().to_string(),
            "returned handle is the head's checkpoint handle",
        );
        Ok(())
    }

    #[tokio::test]
    async fn genesis_handle_inherits_no_history() -> Result<(), Box<dyn std::error::Error>> {
        let mut history = History::new();
        history.append(user_message("one"));
        let head = history.append(user_message("two"));

        let spawner = Arc::new(RecordingSpawner::new());
        let ctx = context(Some(head), Arc::new(ToolRegistry::new()), spawner.clone());
        let tool = Fork::new();

        let value = tool
            .call_with_context(
                json!({
                    "instruction": "go",
                    "handle": CheckpointHandle::genesis().to_string(),
                }),
                &ctx,
            )
            .await
            .expect("fork succeeds");

        let specs = spawner.specs();
        assert_eq!(specs.len(), 1);
        assert!(
            specs[0].history_prefix.is_none(),
            "genesis handle inherits the empty prefix",
        );
        assert_eq!(
            value["handle"].as_str().ok_or("handle is a string")?,
            CheckpointHandle::genesis().to_string(),
        );
        Ok(())
    }

    #[tokio::test]
    async fn real_boundary_inherits_prefix_up_to_and_including_it()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut history = History::new();
        history.append(user_message("one"));
        let boundary = history.append(user_message("two"));
        history.append(user_message("three"));
        let head = history.head().cloned().ok_or("history has a head")?;

        let spawner = Arc::new(RecordingSpawner::new());
        let ctx = context(Some(head), Arc::new(ToolRegistry::new()), spawner.clone());
        let tool = Fork::new();

        let value = tool
            .call_with_context(
                json!({
                    "instruction": "go",
                    "handle": boundary.checkpoint_handle().to_string(),
                }),
                &ctx,
            )
            .await
            .expect("fork succeeds");

        let specs = spawner.specs();
        let prefix = specs[0]
            .history_prefix
            .as_ref()
            .ok_or("boundary handle inherits a prefix")?;
        assert_eq!(
            prefix.id(),
            boundary.id(),
            "prefix head is the named boundary node, dropping later blocks",
        );
        assert_eq!(
            value["handle"].as_str().ok_or("handle is a string")?,
            boundary.checkpoint_handle().to_string(),
        );
        Ok(())
    }

    #[tokio::test]
    async fn tools_narrows_to_named_subset() -> Result<(), Box<dyn std::error::Error>> {
        let mut parent_tools = ToolRegistry::new();
        parent_tools.register(Arc::new(EchoTool::new("echo-a")))?;
        parent_tools.register(Arc::new(EchoTool::new("echo-b")))?;

        let spawner = Arc::new(RecordingSpawner::new());
        let ctx = context(None, Arc::new(parent_tools), spawner.clone());
        let tool = Fork::new();

        tool.call_with_context(json!({ "instruction": "go", "tools": ["echo-a"] }), &ctx)
            .await
            .expect("fork succeeds");

        let specs = spawner.specs();
        let names: Vec<String> = specs[0]
            .tools
            .list()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["echo-a".to_string()],
            "fork sees only the subset"
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_handle_errors_and_spawns_nothing() -> Result<(), Box<dyn std::error::Error>> {
        let mut history = History::new();
        let head = history.append(user_message("one"));

        let spawner = Arc::new(RecordingSpawner::new());
        let ctx = context(Some(head), Arc::new(ToolRegistry::new()), spawner.clone());
        let tool = Fork::new();

        let err = tool
            .call_with_context(
                json!({ "instruction": "go", "handle": "not-a-handle" }),
                &ctx,
            )
            .await
            .expect_err("malformed handle must error");

        let value = execution_value(err)?;
        assert_eq!(value["kind"], "invalid_handle");
        assert!(
            spawner.specs().is_empty(),
            "no agent may be spawned on an invalid handle",
        );
        Ok(())
    }

    #[tokio::test]
    async fn unknown_handle_errors_and_spawns_nothing() -> Result<(), Box<dyn std::error::Error>> {
        use crate::history::NodeId;

        let mut history = History::new();
        let head = history.append(user_message("one"));

        let spawner = Arc::new(RecordingSpawner::new());
        let ctx = context(Some(head), Arc::new(ToolRegistry::new()), spawner.clone());
        let tool = Fork::new();

        let orphan = CheckpointHandle::from_node_id(NodeId::new());
        let err = tool
            .call_with_context(
                json!({ "instruction": "go", "handle": orphan.to_string() }),
                &ctx,
            )
            .await
            .expect_err("unknown handle must error");

        let value = execution_value(err)?;
        assert_eq!(value["kind"], "invalid_handle");
        assert!(
            spawner.specs().is_empty(),
            "no agent may be spawned on an unknown handle",
        );
        Ok(())
    }

    #[tokio::test]
    async fn unknown_tool_name_errors_and_spawns_nothing() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut parent_tools = ToolRegistry::new();
        parent_tools.register(Arc::new(EchoTool::new("echo-a")))?;

        let spawner = Arc::new(RecordingSpawner::new());
        let ctx = context(None, Arc::new(parent_tools), spawner.clone());
        let tool = Fork::new();

        let err = tool
            .call_with_context(json!({ "instruction": "go", "tools": ["nonesuch"] }), &ctx)
            .await
            .expect_err("a tool outside the parent set must error");

        let value = execution_value(err)?;
        assert_eq!(value["kind"], "unknown_tool");
        assert_eq!(value["name"], "nonesuch");
        assert!(
            spawner.specs().is_empty(),
            "no agent may be spawned when a requested tool is unknown",
        );
        Ok(())
    }

    #[tokio::test]
    async fn return_value_carries_agent_id_and_handle() -> Result<(), Box<dyn std::error::Error>> {
        let mut history = History::new();
        let head = history.append(user_message("only"));

        let spawner = Arc::new(RecordingSpawner::new());
        let ctx = context(Some(head.clone()), Arc::new(ToolRegistry::new()), spawner);
        let tool = Fork::new();

        let value = tool
            .call_with_context(json!({ "instruction": "go" }), &ctx)
            .await
            .expect("fork succeeds");

        let agent_id = value["agent_id"].as_str().ok_or("agent_id is a string")?;
        assert!(
            uuid::Uuid::parse_str(agent_id).is_ok(),
            "agent_id must be a parseable id, got {agent_id:?}",
        );
        assert_eq!(
            value["handle"].as_str().ok_or("handle is a string")?,
            head.checkpoint_handle().to_string(),
        );
        Ok(())
    }

    #[tokio::test]
    async fn call_without_context_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let tool = Fork::new();
        let err = tool
            .call(json!({ "instruction": "go" }))
            .await
            .expect_err("fork cannot run without the calling context");
        assert!(matches!(err, ToolError::InvalidInput(_)));
        Ok(())
    }
}
