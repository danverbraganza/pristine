//! Tool trait, error type, and registry for agent-invocable capabilities.

use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug)]
pub enum ToolError {
    NotFound(String),
    InvalidInput(String),
    Execution(String),
    AlreadyRegistered(String),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::NotFound(name) => write!(f, "tool not found: {name}"),
            ToolError::InvalidInput(msg) => write!(f, "invalid tool input: {msg}"),
            ToolError::Execution(msg) => write!(f, "tool execution error: {msg}"),
            ToolError::AlreadyRegistered(name) => write!(f, "tool already registered: {name}"),
        }
    }
}

impl std::error::Error for ToolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

#[jsonrpsee::core::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> &serde_json::Value;
    async fn call(&self, input: serde_json::Value) -> Result<serde_json::Value, ToolError>;
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
    fn duplicate_registration_is_rejected() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(EchoTool::new("echo")))
            .expect("first registration succeeds");
        let err = registry
            .register(Arc::new(EchoTool::new("echo")))
            .expect_err("second registration fails");
        match err {
            ToolError::AlreadyRegistered(name) => assert_eq!(name, "echo"),
            other => panic!("expected AlreadyRegistered, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_not_found() {
        let registry = ToolRegistry::new();
        let err = registry
            .dispatch("missing", serde_json::Value::Null)
            .await
            .expect_err("dispatch on unknown name fails");
        match err {
            ToolError::NotFound(name) => assert_eq!(name, "missing"),
            other => panic!("expected NotFound, got {other:?}"),
        }
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
