//! Tool trait, error type, and registry for agent-invocable capabilities.

use std::collections::HashMap;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::agent::SystemPrompt;
use crate::harness::AgentSpawner;
use crate::history::{AgentId, HistoryNode};
use crate::model::{ARModel, ModelRole};

/// Read-only, per-dispatch view of the calling Agent's live runtime state.
///
/// The Agent assembles one of these at each tool dispatch and hands it to the
/// tool by shared reference. It carries what an agent-aware tool (e.g. a Fork
/// or Exit tool) needs that construction-time context cannot: the calling
/// agent's identity, its current `History` head, its `SystemPrompt`, its model
/// assignment and tool set, a spawner for creating peer Agents, and a self-stop
/// handle. Tools that do not need any of this ignore the context and go through
/// the default [`Tool::call_with_context`] impl, which forwards to
/// [`Tool::call`].
pub struct ToolCallContext {
    agent_id: AgentId,
    history_head: Option<Arc<HistoryNode>>,
    system_prompt: SystemPrompt,
    models: HashMap<ModelRole, Arc<dyn ARModel>>,
    tools: Arc<ToolRegistry>,
    spawner: Arc<dyn AgentSpawner>,
    stop_token: CancellationToken,
}

impl ToolCallContext {
    /// Assemble a context from the calling Agent's fields. Called by the Agent
    /// at each tool dispatch; not part of the public tool surface.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        agent_id: AgentId,
        history_head: Option<Arc<HistoryNode>>,
        system_prompt: SystemPrompt,
        models: HashMap<ModelRole, Arc<dyn ARModel>>,
        tools: Arc<ToolRegistry>,
        spawner: Arc<dyn AgentSpawner>,
        stop_token: CancellationToken,
    ) -> Self {
        Self {
            agent_id,
            history_head,
            system_prompt,
            models,
            tools,
            spawner,
            stop_token,
        }
    }

    /// The calling Agent's id.
    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    /// The calling Agent's current `History` head, or `None` when its log is
    /// empty. The node's parent chain is the full prefix; a tool can seed a
    /// `History` from it and use `History::resolve` to address checkpoints.
    pub fn history_head(&self) -> Option<&Arc<HistoryNode>> {
        self.history_head.as_ref()
    }

    /// The calling Agent's `SystemPrompt`.
    pub fn system_prompt(&self) -> &SystemPrompt {
        &self.system_prompt
    }

    /// The model bound to `role`, if any.
    pub fn model(&self, role: ModelRole) -> Option<&Arc<dyn ARModel>> {
        self.models.get(&role)
    }

    /// The calling Agent's full model assignment.
    pub fn models(&self) -> &HashMap<ModelRole, Arc<dyn ARModel>> {
        &self.models
    }

    /// The calling Agent's tool set.
    pub fn tools(&self) -> &Arc<ToolRegistry> {
        &self.tools
    }

    /// The runtime spawner for creating peer Agents.
    pub fn spawner(&self) -> &Arc<dyn AgentSpawner> {
        &self.spawner
    }

    /// The calling Agent's self-stop handle: its per-agent cancellation child
    /// token. Cancelling it terminates only this Agent.
    pub fn stop_token(&self) -> &CancellationToken {
        &self.stop_token
    }

    /// Request that the calling Agent stop, terminating only this Agent's task.
    pub fn stop(&self) {
        self.stop_token.cancel();
    }
}

#[derive(Debug)]
pub enum ToolError {
    NotFound(String),
    InvalidInput(String),
    Execution(serde_json::Value),
    AlreadyRegistered(String),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::NotFound(name) => write!(f, "tool not found: {name}"),
            ToolError::InvalidInput(msg) => write!(f, "invalid tool input: {msg}"),
            ToolError::Execution(value) => {
                let rendered = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
                write!(f, "tool execution error: {rendered}")
            }
            ToolError::AlreadyRegistered(name) => write!(f, "tool already registered: {name}"),
        }
    }
}

impl std::error::Error for ToolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

/// Wraps a per-tool typed error enum into the portable
/// `ToolError::Execution(Value)` carrier. Each built-in tool defines its own
/// dialect enum (kept local) and routes it through this helper so the
/// JSON-shape wrapping has a single implementation.
pub(crate) fn execution_err<E: serde::Serialize>(e: E) -> ToolError {
    let value =
        serde_json::to_value(e).unwrap_or_else(|_| serde_json::json!({"kind": "internal_error"}));
    ToolError::Execution(value)
}

#[jsonrpsee::core::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> &serde_json::Value;
    async fn call(&self, input: serde_json::Value) -> Result<serde_json::Value, ToolError>;

    /// Invoke the tool with the calling Agent's live [`ToolCallContext`].
    ///
    /// The default forwards to [`call`](Tool::call), ignoring the context, so
    /// tools that do not need agent-aware state require no changes. Agent-aware
    /// tools override this to read the calling Agent's state or drive it (e.g.
    /// self-stop, spawning a peer).
    async fn call_with_context(
        &self,
        input: serde_json::Value,
        _ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, ToolError> {
        self.call(input).await
    }
}

/// Owns the set of `Tool`s available to an Agent. Names are unique; attempts to
/// register a duplicate are rejected rather than silently overwriting an
/// existing entry.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolError> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(ToolError::AlreadyRegistered(name));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn list(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.values().cloned().collect()
    }

    pub async fn dispatch(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let tool = self
            .get(name)
            .ok_or_else(|| ToolError::NotFound(name.to_string()))?;
        tool.call(input).await
    }

    /// Dispatch by name, supplying the calling Agent's [`ToolCallContext`].
    ///
    /// The Agent's dispatch site always routes through here; tools that do not
    /// need the context fall through to the default [`Tool::call_with_context`]
    /// impl and behave identically to [`dispatch`](Self::dispatch).
    pub async fn dispatch_with_context(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<serde_json::Value, ToolError> {
        let tool = self
            .get(name)
            .ok_or_else(|| ToolError::NotFound(name.to_string()))?;
        tool.call_with_context(input, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EchoTool;

    fn assert_std_error<T: std::error::Error + Send + Sync + 'static>() {}

    #[test]
    fn tool_error_is_standard_error_trait_object() {
        assert_std_error::<ToolError>();
    }

    #[tokio::test]
    async fn register_and_dispatch_happy_path() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(EchoTool::new("echo")))
            .expect("first registration succeeds");

        let result = registry
            .dispatch("echo", serde_json::json!({ "hello": "world" }))
            .await
            .expect("dispatch succeeds");
        assert_eq!(result, serde_json::json!({ "echo": { "hello": "world" } }));
    }

    #[test]
    fn duplicate_registration_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(EchoTool::new("echo")))
            .expect("first registration succeeds");
        let err = registry
            .register(Arc::new(EchoTool::new("echo")))
            .expect_err("second registration fails");
        match err {
            ToolError::AlreadyRegistered(name) => assert_eq!(name, "echo"),
            other => return Err(format!("expected AlreadyRegistered, got {other:?}").into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_not_found() -> Result<(), Box<dyn std::error::Error>> {
        let registry = ToolRegistry::new();
        let err = registry
            .dispatch("missing", serde_json::Value::Null)
            .await
            .expect_err("dispatch on unknown name fails");
        match err {
            ToolError::NotFound(name) => assert_eq!(name, "missing"),
            other => return Err(format!("expected NotFound, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn list_returns_all_registered_tools() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(EchoTool::new("echo-a")))
            .expect("register echo-a");
        registry
            .register(Arc::new(EchoTool::new("echo-b")))
            .expect("register echo-b");

        let mut names: Vec<String> = registry
            .list()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["echo-a".to_string(), "echo-b".to_string()]);
    }
}
