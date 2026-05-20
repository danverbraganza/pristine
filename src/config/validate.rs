//! Intra-topology validation: every tool name an agent references must be
//! declared in the topology's `[tools]` table, and an agent must not list the
//! same tool name more than once. Both invariants are reported per-agent and
//! per-tool-name; duplicates collapse to one error per offending name even when
//! the agent lists the same tool three or more times.
//!
//! This layer does NOT check that tool names correspond to runtime `Tool`
//! impls in `HarnessBuilder` — that mapping lives at the binary boundary.

use std::collections::HashSet;

use crate::config::error::{ConfigError, ConfigErrors};
use crate::config::topology::TopologyConfig;

/// Validate every `agent.tools` entry in `topology` against `topology.tools`.
///
/// Appends a `ConfigError::UndeclaredTool` for each `(agent, tool)` pair whose
/// `tool` is not a key of `topology.tools`, and a `ConfigError::DuplicateToolRef`
/// for each `(agent, tool)` pair where `tool` appears more than once in the
/// agent's `tools` array. Duplicates are reported once per agent/tool pair, not
/// once per occurrence; the two invariants are independent, so a duplicated
/// undeclared tool collects both errors.
pub fn validate_tool_refs(topology: &TopologyConfig, errors: &mut ConfigErrors) {
    for agent in &topology.agents {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut duplicates_reported: HashSet<&str> = HashSet::new();
        for tool_name in &agent.tools {
            let already_seen = !seen.insert(tool_name.as_str());
            if already_seen && duplicates_reported.insert(tool_name.as_str()) {
                errors.push(ConfigError::DuplicateToolRef {
                    agent: agent.name.clone(),
                    tool: tool_name.clone(),
                });
            }
        }
        let mut undeclared_reported: HashSet<&str> = HashSet::new();
        for tool_name in &agent.tools {
            if !topology.tools.contains_key(tool_name)
                && undeclared_reported.insert(tool_name.as_str())
            {
                errors.push(ConfigError::UndeclaredTool {
                    agent: agent.name.clone(),
                    tool: tool_name.clone(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::topology::{AgentConfig, ToolConfig};
    use std::collections::HashMap;

    fn agent(name: &str, tools: &[&str]) -> AgentConfig {
        AgentConfig {
            name: name.to_string(),
            model: "default".to_string(),
            system_prompt: format!("prompt for {name}"),
            tools: tools.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn topology(agents: Vec<AgentConfig>, tool_keys: &[&str]) -> TopologyConfig {
        let mut tools = HashMap::new();
        for key in tool_keys {
            tools.insert(
                (*key).to_string(),
                ToolConfig::Builtin {
                    builtin: (*key).to_string(),
                },
            );
        }
        TopologyConfig { agents, tools }
    }

    #[test]
    fn all_declared_tools_pass() {
        let topo = topology(
            vec![agent("default", &["read", "write"])],
            &["read", "write"],
        );
        let mut errors = ConfigErrors::new();
        validate_tool_refs(&topo, &mut errors);
        assert!(errors.is_empty(), "expected no errors, got {errors}");
    }

    #[test]
    fn undeclared_tool_collects_error() {
        let topo = topology(vec![agent("default", &["mystery"])], &[]);
        let mut errors = ConfigErrors::new();
        validate_tool_refs(&topo, &mut errors);
        assert_eq!(errors.len(), 1);
        match &errors.as_slice()[0] {
            ConfigError::UndeclaredTool { agent, tool } => {
                assert_eq!(agent, "default");
                assert_eq!(tool, "mystery");
            }
            other => panic!("expected UndeclaredTool, got {other:?}"),
        }
    }

    #[test]
    fn empty_tools_list_is_allowed() {
        let topo = topology(vec![agent("default", &[])], &["read"]);
        let mut errors = ConfigErrors::new();
        validate_tool_refs(&topo, &mut errors);
        assert!(errors.is_empty(), "expected no errors, got {errors}");
    }

    #[test]
    fn duplicate_within_one_agent_collects_error() {
        let topo = topology(vec![agent("default", &["read", "read"])], &["read"]);
        let mut errors = ConfigErrors::new();
        validate_tool_refs(&topo, &mut errors);
        assert_eq!(errors.len(), 1);
        match &errors.as_slice()[0] {
            ConfigError::DuplicateToolRef { agent, tool } => {
                assert_eq!(agent, "default");
                assert_eq!(tool, "read");
            }
            other => panic!("expected DuplicateToolRef, got {other:?}"),
        }
        for err in errors.as_slice() {
            if let ConfigError::UndeclaredTool { tool, .. } = err {
                panic!("declared duplicate 'read' should not be flagged as undeclared: {tool}");
            }
        }
    }

    #[test]
    fn triple_duplicate_records_only_once() {
        let topo = topology(vec![agent("default", &["read", "read", "read"])], &["read"]);
        let mut errors = ConfigErrors::new();
        validate_tool_refs(&topo, &mut errors);
        let dup_count = errors
            .as_slice()
            .iter()
            .filter(|e| matches!(e, ConfigError::DuplicateToolRef { tool, .. } if tool == "read"))
            .count();
        assert_eq!(dup_count, 1, "expected exactly one duplicate error");
        assert_eq!(errors.len(), 1, "no other errors should fire");
    }

    #[test]
    fn multiple_agents_independent() {
        let topo = topology(
            vec![
                agent("dup_agent", &["read", "read"]),
                agent("undecl_agent", &["mystery"]),
            ],
            &["read"],
        );
        let mut errors = ConfigErrors::new();
        validate_tool_refs(&topo, &mut errors);
        assert_eq!(errors.len(), 2);

        let mut found_dup = false;
        let mut found_undecl = false;
        for err in errors.as_slice() {
            match err {
                ConfigError::DuplicateToolRef { agent, tool } => {
                    assert_eq!(agent, "dup_agent");
                    assert_eq!(tool, "read");
                    found_dup = true;
                }
                ConfigError::UndeclaredTool { agent, tool } => {
                    assert_eq!(agent, "undecl_agent");
                    assert_eq!(tool, "mystery");
                    found_undecl = true;
                }
                other => panic!("unexpected error variant: {other:?}"),
            }
        }
        assert!(found_dup, "expected a DuplicateToolRef on dup_agent");
        assert!(found_undecl, "expected an UndeclaredTool on undecl_agent");
    }

    #[test]
    fn mixed_undeclared_and_duplicate_on_same_agent() {
        let topo = topology(
            vec![agent("default", &["read", "read", "mystery"])],
            &["read"],
        );
        let mut errors = ConfigErrors::new();
        validate_tool_refs(&topo, &mut errors);
        assert_eq!(errors.len(), 2);

        let dup_for_read = errors
            .as_slice()
            .iter()
            .filter(|e| matches!(e, ConfigError::DuplicateToolRef { tool, agent, .. } if tool == "read" && agent == "default"))
            .count();
        assert_eq!(dup_for_read, 1, "expected one DuplicateToolRef for 'read'");

        let undecl_for_mystery = errors
            .as_slice()
            .iter()
            .filter(|e| matches!(e, ConfigError::UndeclaredTool { tool, agent, .. } if tool == "mystery" && agent == "default"))
            .count();
        assert_eq!(
            undecl_for_mystery, 1,
            "expected one UndeclaredTool for 'mystery'"
        );

        let dup_for_mystery = errors
            .as_slice()
            .iter()
            .filter(
                |e| matches!(e, ConfigError::DuplicateToolRef { tool, .. } if tool == "mystery"),
            )
            .count();
        assert_eq!(
            dup_for_mystery, 0,
            "'mystery' appears only once, no duplicate error expected"
        );
    }
}
