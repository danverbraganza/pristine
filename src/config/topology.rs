//! Topology configuration — agents, tools, prompts.
//!
//! Mirrors the embedded `default.toml`. Provider-agnostic: agents reference
//! model aliases by name (resolved later against `AuthConfig`) and tools by
//! registry key. All structs reject unknown fields so misspellings surface as
//! parse errors rather than silently-ignored TOML.

use std::collections::HashMap;

use serde::Deserialize;

/// Root of the topology file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyConfig {
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    #[serde(default)]
    pub tools: HashMap<String, ToolConfig>,
}

/// One entry of `[[agents]]` in the topology file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    /// Model alias resolved against `AuthConfig.models`.
    pub model: String,
    pub system_prompt: String,
    #[serde(default)]
    pub tools: Vec<String>,
}

/// One entry of `[tools.X]` in the topology file. The `type` discriminator
/// selects the variant; per-variant fields are decoded inline. v1 ships only
/// `Builtin`; future variants (MCP, scripted, etc.) plug in here.
///
/// `ToolConfig` and `ToolKind` are intentionally the same type: serde's
/// `flatten` is incompatible with `deny_unknown_fields`, so the canonical
/// shape is a directly-tagged enum.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolConfig {
    /// References a built-in tool by its `Tool::name()` (e.g. `"read"`,
    /// `"exec_bash"`).
    Builtin { builtin: String },
}

/// Alias retained so downstream beads can name the variant set explicitly.
pub type ToolKind = ToolConfig;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_rejects_unknown_field() {
        let toml = r#"
name = "default"
model = "default"
system_prompt = "hi"
tools = []
extra = "nope"
"#;
        let err = toml::from_str::<AgentConfig>(toml).expect_err("unknown field is rejected");
        assert!(
            err.to_string().contains("extra"),
            "error mentions the unknown field: {err}"
        );
    }

    #[test]
    fn topology_config_rejects_unknown_top_level_field() {
        let toml = r#"
agents = []
rogue = 1
"#;
        let err = toml::from_str::<TopologyConfig>(toml).expect_err("unknown field is rejected");
        assert!(
            err.to_string().contains("rogue"),
            "error mentions the unknown field: {err}"
        );
    }

    #[test]
    fn tool_kind_builtin_round_trip() {
        let toml = r#"
type = "builtin"
builtin = "read"
"#;
        let cfg: ToolConfig = toml::from_str(toml).expect("builtin tool deserializes");
        match cfg {
            ToolConfig::Builtin { builtin } => assert_eq!(builtin, "read"),
        }
    }
}
