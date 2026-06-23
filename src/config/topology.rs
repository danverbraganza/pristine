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
    /// (skills disabled); `Some` means the block exists. At assembly time the
    /// block is resolved via [`SkillsConfig::resolve`] into
    /// `Config.skills: Option<ResolvedSkillsConfig>`, where `None` is the sole
    /// representation of "disabled".
    #[serde(default)]
    pub skills: Option<SkillsConfig>,
}

/// The `[skills]` topology block: the serde deserialization target for a
/// present block. `enabled = false` is the explicit kill-switch; an omitted
/// `enabled` means enabled. Path arrays carry unresolved strings — `~` and cwd
/// expansion happen at scan time, not at parse time. The present/absent and
/// enabled/disabled decisions are collapsed by [`SkillsConfig::resolve`] into an
/// `Option<ResolvedSkillsConfig>`; the resolved type has no `enabled` field, so
/// "disabled" is unrepresentable past resolution.
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillsConfig {
    /// `None` when omitted from a present block (interpreted as enabled).
    /// `Some(false)` is the kill-switch.
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

/// Resolved skills configuration: the inert value carried by `Config.skills`
/// once the kill-switch has been applied. There is no `enabled` field — its
/// mere presence (`Some`) means skills are enabled, so "disabled" is
/// unrepresentable. Path arrays remain unresolved strings; `~` and cwd
/// expansion happen at scan time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedSkillsConfig {
    /// User-scope scan paths. `None` selects the conventional defaults; a
    /// supplied list REPLACES the defaults (no merge).
    pub user_paths: Option<Vec<String>>,
    /// Project-scope scan paths. `None` selects the conventional defaults; a
    /// supplied list REPLACES the defaults (no merge).
    pub project_paths: Option<Vec<String>>,
    /// Exact skill names to exclude from discovery.
    pub disabled: Vec<String>,
}

/// Conventional user-scope scan paths used when `user_paths` is omitted.
/// Unresolved; `~` expansion happens at scan time.
const DEFAULT_USER_PATHS: [&str; 2] = ["~/.agents/skills", "~/.pristine/skills"];

/// Conventional project-scope scan paths used when `project_paths` is omitted.
/// Unresolved; cwd resolution happens at scan time.
const DEFAULT_PROJECT_PATHS: [&str; 2] = [".agents/skills", ".pristine/skills"];

impl SkillsConfig {
    /// Apply the kill-switch: `enabled = false` resolves to `None` (disabled);
    /// an omitted or `true` `enabled` resolves to `Some(ResolvedSkillsConfig)`.
    /// Block presence is the caller's concern — an absent block never reaches
    /// this method.
    pub fn resolve(self) -> Option<ResolvedSkillsConfig> {
        if self.enabled == Some(false) {
            return None;
        }
        Some(ResolvedSkillsConfig {
            user_paths: self.user_paths,
            project_paths: self.project_paths,
            disabled: self.disabled,
        })
    }
}

impl ResolvedSkillsConfig {
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
    fn skills_config_default_parses_clean() {
        let cfg = SkillsConfig::default();
        assert_eq!(cfg.enabled, None);
        assert_eq!(cfg.user_paths, None);
        assert_eq!(cfg.project_paths, None);
        assert!(cfg.disabled.is_empty());
    }

    #[test]
    fn skills_config_empty_block_parses_with_enabled_none() {
        // A present block with no fields: `enabled` stays None at parse time;
        // the enabled decision is applied by `resolve`.
        let cfg: SkillsConfig = toml::from_str("").expect("empty skills block parses");
        assert_eq!(cfg.enabled, None);
    }

    #[test]
    fn skills_config_resolve_enabled_omitted_is_some() {
        let cfg = SkillsConfig::default();
        assert!(
            cfg.resolve().is_some(),
            "omitted `enabled` resolves to Some (enabled)"
        );
    }

    #[test]
    fn skills_config_resolve_enabled_false_is_none() {
        let cfg: SkillsConfig = toml::from_str("enabled = false").expect("parses");
        assert_eq!(cfg.enabled, Some(false));
        assert!(
            cfg.resolve().is_none(),
            "`enabled = false` resolves to None (disabled)"
        );
    }

    #[test]
    fn skills_config_resolve_enabled_true_is_some() {
        let cfg: SkillsConfig = toml::from_str("enabled = true").expect("parses");
        assert_eq!(cfg.enabled, Some(true));
        assert!(cfg.resolve().is_some());
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
    fn resolved_skills_config_effective_paths_default_when_omitted() {
        let cfg = ResolvedSkillsConfig::default();
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
    fn resolved_skills_config_custom_paths_replace_defaults() {
        let cfg = SkillsConfig {
            user_paths: Some(vec!["~/custom/user".to_string()]),
            project_paths: Some(vec!["./custom/project".to_string()]),
            ..SkillsConfig::default()
        }
        .resolve()
        .expect("omitted enabled resolves to Some");
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
