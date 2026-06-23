//! ActivateSkill: resolves a skill by name and returns its frontmatter-stripped
//! body. The catalog of available skills lives in the system prompt (the sole
//! tier-1 disclosure surface), so this tool's description deliberately does not
//! enumerate skills.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::skills::SkillsRegistrySource;
use crate::tool::{Tool, ToolError, execution_err};

#[derive(serde::Deserialize)]
struct ActivateSkillInput {
    name: String,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ActivateSkillError {
    InvalidInput { reason: String },
    UnknownSkill { name: String, known: Vec<String> },
}

/// Built-in tool that activates a skill by name. Holds a shared handle to the
/// skills registry so activation resolves against the same catalog rendered in
/// the system prompt.
pub struct ActivateSkill {
    schema: Value,
    registry: Arc<dyn SkillsRegistrySource>,
}

impl ActivateSkill {
    pub fn new(registry: Arc<dyn SkillsRegistrySource>) -> Self {
        Self {
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }),
            registry,
        }
    }
}

#[jsonrpsee::core::async_trait]
impl Tool for ActivateSkill {
    fn name(&self) -> &str {
        "activate_skill"
    }

    fn description(&self) -> &str {
        "Activate one of the available skills by name."
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    async fn call(&self, input: Value) -> Result<Value, ToolError> {
        let parsed: ActivateSkillInput = serde_json::from_value(input).map_err(|e| {
            execution_err(ActivateSkillError::InvalidInput {
                reason: format!("activate_skill requires a string field 'name': {e}"),
            })
        })?;

        match self.registry.get(&parsed.name) {
            Some(record) => Ok(json!({ "body": record.body })),
            None => {
                let known = self
                    .registry
                    .list()
                    .into_iter()
                    .map(|s| s.name)
                    .collect::<Vec<String>>();
                Err(execution_err(ActivateSkillError::UnknownSkill {
                    name: parsed.name,
                    known,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillRecord;
    use crate::test_support::{StubSkillsRegistry, execution_value};
    use std::path::PathBuf;

    fn record(name: &str, body: &str) -> SkillRecord {
        SkillRecord {
            name: name.to_string(),
            description: format!("{name} description"),
            body: body.to_string(),
            directory: PathBuf::from(format!("/skills/{name}")),
        }
    }

    fn tool(records: Vec<SkillRecord>) -> ActivateSkill {
        ActivateSkill::new(Arc::new(StubSkillsRegistry::new(records)))
    }

    #[tokio::test]
    async fn activate_known_skill_returns_stripped_body() {
        let tool = tool(vec![record("alpha", "# Alpha\n\nbody text")]);

        let value = tool
            .call(json!({ "name": "alpha" }))
            .await
            .expect("known skill activates");

        assert_eq!(value, json!({ "body": "# Alpha\n\nbody text" }));
    }

    #[tokio::test]
    async fn activate_unknown_skill_returns_unknown_with_known_list()
    -> Result<(), Box<dyn std::error::Error>> {
        let tool = tool(vec![record("alpha", "a"), record("beta", "b")]);

        let err = tool
            .call(json!({ "name": "gamma" }))
            .await
            .expect_err("unknown skill must error");

        let value = execution_value(err)?;
        assert_eq!(value["kind"], "unknown_skill");
        assert_eq!(value["name"], "gamma");
        let mut known: Vec<String> = value["known"]
            .as_array()
            .ok_or("known is an array")?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        known.sort();
        assert_eq!(known, vec!["alpha".to_string(), "beta".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn activate_missing_name_returns_invalid_input() -> Result<(), Box<dyn std::error::Error>>
    {
        let tool = tool(vec![record("alpha", "a")]);

        let err = tool
            .call(json!({}))
            .await
            .expect_err("missing name must error");

        let value = execution_value(err)?;
        assert_eq!(value["kind"], "invalid_input");
        let reason = value["reason"].as_str().ok_or("reason is a string")?;
        assert!(!reason.is_empty(), "reason should be non-empty");
        Ok(())
    }

    #[tokio::test]
    async fn activate_non_string_name_returns_invalid_input()
    -> Result<(), Box<dyn std::error::Error>> {
        let tool = tool(vec![record("alpha", "a")]);

        let err = tool
            .call(json!({ "name": 42 }))
            .await
            .expect_err("non-string name must error");

        let value = execution_value(err)?;
        assert_eq!(value["kind"], "invalid_input");
        Ok(())
    }
}
