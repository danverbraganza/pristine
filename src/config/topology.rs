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
    /// Optional global `[skills]` block. `None` means no block was present
    /// (skills disabled); `Some` means the block exists. The present/absent
    /// distinction is collapsed into `SkillsConfig::enabled` at assembly time
    /// (see `assemble_config`): block-present resolves to enabled unless
    /// `enabled = false` is explicit.
    #[serde(default)]
    pub skills: Option<SkillsConfig>,
}

/// The `[skills]` topology block. Present-means-enabled: a block that omits
/// `enabled` resolves to enabled; `enabled = false` is the explicit kill-switch.
/// Path arrays carry unresolved strings — `~` and cwd expansion happen at scan
/// time, not at parse time. `default.toml` ships skills-free, so out-of-the-box
/// behavior is unchanged.
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillsConfig {
    /// `None` when omitted from a present block; interpreted as enabled by
    /// `assemble_config` flattening. `Some(false)` is the kill-switch.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// User-scope scan paths. `None` selects the conventional defaults; a
    /// supplied list REPLACES the defaults (no merge).
    #[serde(default)]
    pub user_paths: Option<Vec<String>>,
    /// Project-scope scan paths. `None` selects the conventional defaults; a
    /// supplied list REPLACES the defaults (no merge).
    #[serde(default)]
    pub project_paths: Option<Vec<String>>,
    /// Exact skill names to exclude from discovery.
    #[serde(default)]
    pub disabled: Vec<String>,
}

/// Conventional user-scope scan paths used when `user_paths` is omitted.
/// Unresolved; `~` expansion happens at scan time.
const DEFAULT_USER_PATHS: [&str; 2] = ["~/.agents/skills", "~/.pristine/skills"];

/// Conventional project-scope scan paths used when `project_paths` is omitted.
/// Unresolved; cwd resolution happens at scan time.
const DEFAULT_PROJECT_PATHS: [&str; 2] = [".agents/skills", ".pristine/skills"];

impl SkillsConfig {
    /// Whether skills discovery is enabled. `enabled.unwrap_or(false)`: with the
    /// `assemble_config` flattening (present-block ⇒ `Some(true)`), this yields
    /// present+omitted ⇒ true, present+`false` ⇒ false, absent ⇒ false.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// User-scope scan paths: the supplied list when present, otherwise the
    /// conventional defaults. Strings are unresolved.
    pub fn effective_user_paths(&self) -> Vec<String> {
        match &self.user_paths {
            Some(paths) => paths.clone(),
            None => DEFAULT_USER_PATHS.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Project-scope scan paths: the supplied list when present, otherwise the
    /// conventional defaults. Strings are unresolved.
    pub fn effective_project_paths(&self) -> Vec<String> {
        match &self.project_paths {
            Some(paths) => paths.clone(),
            None => DEFAULT_PROJECT_PATHS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
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
/// `ToolConfig` is a directly-tagged enum because serde's `flatten` is
/// incompatible with `deny_unknown_fields`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolConfig {
    /// References a built-in tool by its `Tool::name()` (e.g. `"read"`,
    /// `"exec_bash"`).
    Builtin { builtin: String },
}

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
    fn skills_config_default_is_disabled() {
        let cfg = SkillsConfig::default();
        assert!(!cfg.is_enabled(), "default SkillsConfig is disabled");
        assert_eq!(cfg.enabled, None);
        assert_eq!(cfg.user_paths, None);
        assert_eq!(cfg.project_paths, None);
        assert!(cfg.disabled.is_empty());
    }

    #[test]
    fn skills_config_empty_block_parses_with_enabled_none() {
        // A present block with no fields: `enabled` stays None at parse time;
        // the present-means-enabled decision is applied in assemble_config.
        let cfg: SkillsConfig = toml::from_str("").expect("empty skills block parses");
        assert_eq!(cfg.enabled, None);
    }

    #[test]
    fn skills_config_enabled_false_parses() {
        let cfg: SkillsConfig = toml::from_str("enabled = false").expect("parses");
        assert_eq!(cfg.enabled, Some(false));
        assert!(!cfg.is_enabled());
    }

    #[test]
    fn skills_config_enabled_true_parses() {
        let cfg: SkillsConfig = toml::from_str("enabled = true").expect("parses");
        assert_eq!(cfg.enabled, Some(true));
        assert!(cfg.is_enabled());
    }

    #[test]
    fn skills_config_rejects_unknown_field() {
        let err = toml::from_str::<SkillsConfig>("bogus = 1").expect_err("unknown field rejected");
        assert!(
            err.to_string().contains("bogus"),
            "error mentions the unknown field: {err}"
        );
    }

    #[test]
    fn skills_config_effective_paths_default_when_omitted() {
        let cfg = SkillsConfig::default();
        assert_eq!(
            cfg.effective_user_paths(),
            vec![
                "~/.agents/skills".to_string(),
                "~/.pristine/skills".to_string()
            ]
        );
        assert_eq!(
            cfg.effective_project_paths(),
            vec![".agents/skills".to_string(), ".pristine/skills".to_string()]
        );
    }

    #[test]
    fn skills_config_custom_paths_replace_defaults() {
        let cfg: SkillsConfig = toml::from_str(
            r#"
user_paths = ["~/custom/user"]
project_paths = ["./custom/project"]
"#,
        )
        .expect("custom paths parse");
        assert_eq!(
            cfg.effective_user_paths(),
            vec!["~/custom/user".to_string()]
        );
        assert_eq!(
            cfg.effective_project_paths(),
            vec!["./custom/project".to_string()]
        );
    }

    #[test]
    fn topology_config_accepts_skills_block() {
        let toml = r#"
agents = []

[skills]
enabled = true
disabled = ["secret"]
"#;
        let cfg: TopologyConfig = toml::from_str(toml).expect("topology with skills parses");
        let skills = cfg.skills.expect("skills block present");
        assert_eq!(skills.enabled, Some(true));
        assert_eq!(skills.disabled, vec!["secret".to_string()]);
    }

    #[test]
    fn topology_config_without_skills_block_is_none() {
        let toml = r#"
agents = []
"#;
        let cfg: TopologyConfig = toml::from_str(toml).expect("topology parses");
        assert_eq!(cfg.skills, None);
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
