//! AddTool: an example [`Tool`](crate::tool::Tool) implementation for SDK
//! consumers writing their own tools.
//!
//! Not registered by the `pristine run` binary (see
//! `src/lib.rs::register_builtin_tools` and `default.toml` for the binary's
//! tool surface). It is exported as `pristine::builtins::AddTool` purely as
//! a reference implementation for embedders.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{Tool, ToolError, execution_err};

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AddError {
    InvalidInput { reason: String },
}

pub struct AddTool {
    schema: Value,
}

impl AddTool {
    pub fn new() -> Self {
        Self {
            schema: json!({
                "type": "object",
                "properties": {
                    "a": {"type": "number"},
                    "b": {"type": "number"}
                },
                "required": ["a", "b"]
            }),
        }
    }
}

impl Default for AddTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct AddInput {
    a: f64,
    b: f64,
}

#[jsonrpsee::core::async_trait]
impl Tool for AddTool {
    fn name(&self) -> &str {
        "add"
    }

    fn description(&self) -> &str {
        "Add two numbers and return their sum."
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    async fn call(&self, input: Value) -> Result<Value, ToolError> {
        let AddInput { a, b } = serde_json::from_value(input).map_err(|e| {
            execution_err(AddError::InvalidInput {
                reason: format!("AddTool requires numeric fields 'a' and 'b': {e}"),
            })
        })?;
        Ok(json!({"sum": a + b}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn add_tool_returns_sum_for_valid_input() {
        let tool = AddTool::new();
        let result = tool
            .call(json!({"a": 2, "b": 3}))
            .await
            .expect("valid input succeeds");
        let sum = result["sum"].as_f64().expect("sum is numeric");
        assert_eq!(sum, 5.0);
    }

    #[tokio::test]
    async fn add_tool_handles_negative_and_fractional() {
        let tool = AddTool::new();
        let result = tool
            .call(json!({"a": -1.5, "b": 4.25}))
            .await
            .expect("valid input succeeds");
        let sum = result["sum"].as_f64().expect("sum is numeric");
        assert_eq!(sum, 2.75);
    }

    #[tokio::test]
    async fn add_tool_rejects_missing_fields() -> Result<(), Box<dyn std::error::Error>> {
        let tool = AddTool::new();
        let result = tool.call(json!({"a": 1})).await;
        let value = match result {
            Err(ToolError::Execution(v)) => v,
            other => return Err(format!("expected Execution(value), got {other:?}").into()),
        };
        assert_eq!(value["kind"], "invalid_input");
        let reason = value["reason"].as_str().expect("reason is a string");
        assert!(
            reason.contains("AddTool requires numeric fields"),
            "unexpected reason: {reason}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn add_tool_rejects_non_numeric_fields() -> Result<(), Box<dyn std::error::Error>> {
        let tool = AddTool::new();
        let result = tool.call(json!({"a": "hello", "b": 1})).await;
        let value = match result {
            Err(ToolError::Execution(v)) => v,
            other => return Err(format!("expected Execution(value), got {other:?}").into()),
        };
        assert_eq!(value["kind"], "invalid_input");
        let reason = value["reason"].as_str().expect("reason is a string");
        assert!(
            reason.contains("AddTool requires numeric fields"),
            "unexpected reason: {reason}"
        );
        Ok(())
    }

    #[test]
    fn add_tool_schema_advertises_required_fields() {
        let tool = AddTool::new();
        let schema = tool.input_schema();
        let required = schema["required"].as_array().expect("required is array");
        let names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap_or("")).collect();
        assert!(names.contains(&"a") && names.contains(&"b"));
    }
}
