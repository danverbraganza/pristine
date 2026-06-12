//! Model-alias resolution between a parsed topology and auth pair.
//!
//! Walks every `agent.model = "X"` in the topology and looks up `X` in
//! `auth.models`. Resolved agents carry the agent identity (`name`,
//! `system_prompt`, `tools`) alongside a `ResolvedModel` that names the
//! provider entry, model name, and api key. Dangling aliases collect a
//! `ConfigError::DanglingAlias` and skip the agent; extra aliases declared in
//! auth but unused by topology are intentionally not errors (auth files are
//! shared across topologies). Provider-config existence is not validated at
//! this layer — that check belongs to the binary's HarnessBuilder pass.

use crate::config::auth::AuthConfig;
use crate::config::error::{ConfigError, ConfigErrors};
use crate::config::topology::TopologyConfig;

/// One topology agent paired with its resolved model alias.
#[derive(Debug, Clone)]
pub struct ResolvedAgent {
    pub name: String,
    pub system_prompt: String,
    pub tools: Vec<String>,
    pub model: ResolvedModel,
}

/// The product of resolving an `agent.model` string against
/// `AuthConfig.models`. Carries the alias key for diagnostics plus the
/// fields needed by the binary to build an `ARModel`. The `provider_name`
/// is left as a string here; resolving it against `AuthConfig.providers`
/// (or the runtime `ProviderRegistry`) happens later, at HarnessBuilder
/// time.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub alias: String,
    pub provider_name: String,
    pub model_name: String,
    pub api_key: String,
}

/// Resolve every `agent.model` in `topology` against `auth.models`.
///
/// Returns one `ResolvedAgent` per topology agent whose alias is present in
/// `auth.models`. Agents whose alias is absent are skipped and a matching
/// `ConfigError::DanglingAlias` is appended to `errors`. Extra entries in
/// `auth.models` that no agent references are not errors.
pub fn resolve_aliases(
    topology: &TopologyConfig,
    auth: &AuthConfig,
    errors: &mut ConfigErrors,
) -> Vec<ResolvedAgent> {
    let mut resolved = Vec::with_capacity(topology.agents.len());
    for agent in &topology.agents {
        match auth.models.get(&agent.model) {
            Some(alias) => {
                resolved.push(ResolvedAgent {
                    name: agent.name.clone(),
                    system_prompt: agent.system_prompt.clone(),
                    tools: agent.tools.clone(),
                    model: ResolvedModel {
                        alias: agent.model.clone(),
                        provider_name: alias.provider.clone(),
                        model_name: alias.model_name.clone(),
                        api_key: alias.api_key.clone(),
                    },
                });
            }
            None => {
                errors.push(ConfigError::DanglingAlias {
                    alias: agent.model.clone(),
                });
            }
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::auth::{ModelAliasConfig, ProviderConfig};
    use crate::config::topology::AgentConfig;
    use std::collections::HashMap;

    fn agent(name: &str, model: &str, tools: &[&str]) -> AgentConfig {
        AgentConfig {
            name: name.to_string(),
            model: model.to_string(),
            system_prompt: format!("prompt for {name}"),
            tools: tools.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn alias(provider: &str, model_name: &str, api_key: &str) -> ModelAliasConfig {
        ModelAliasConfig {
            provider: provider.to_string(),
            model_name: model_name.to_string(),
            api_key: api_key.to_string(),
        }
    }

    fn topology_with(agents: Vec<AgentConfig>) -> TopologyConfig {
        TopologyConfig {
            agents,
            tools: HashMap::new(),
        }
    }

    fn auth_with(models: HashMap<String, ModelAliasConfig>) -> AuthConfig {
        let mut providers = HashMap::new();
        providers.insert(
            "anthropic".to_string(),
            ProviderConfig::Anthropic { base_url: None },
        );
        AuthConfig { providers, models }
    }

    #[test]
    fn resolves_single_agent_against_matching_alias() {
        let topology = topology_with(vec![agent("default", "default", &["read"])]);
        let mut models = HashMap::new();
        models.insert(
            "default".to_string(),
            alias("anthropic", "claude-sonnet-4-6", "sk-foo"),
        );
        let auth = auth_with(models);

        let mut errors = ConfigErrors::new();
        let resolved = resolve_aliases(&topology, &auth, &mut errors);

        assert!(errors.is_empty());
        assert_eq!(resolved.len(), 1);
        let r = &resolved[0];
        assert_eq!(r.name, "default");
        assert_eq!(r.system_prompt, "prompt for default");
        assert_eq!(r.tools, vec!["read".to_string()]);
        assert_eq!(r.model.alias, "default");
        assert_eq!(r.model.provider_name, "anthropic");
        assert_eq!(r.model.model_name, "claude-sonnet-4-6");
        assert_eq!(r.model.api_key, "sk-foo");
    }

    #[test]
    fn dangling_alias_collects_error_and_skips_agent() -> Result<(), Box<dyn std::error::Error>> {
        let topology = topology_with(vec![agent("default", "missing", &[])]);
        let auth = auth_with(HashMap::new());

        let mut errors = ConfigErrors::new();
        let resolved = resolve_aliases(&topology, &auth, &mut errors);

        assert!(resolved.is_empty());
        assert_eq!(errors.len(), 1);
        match &errors.as_slice()[0] {
            ConfigError::DanglingAlias { alias } => assert_eq!(alias, "missing"),
            other => return Err(format!("expected DanglingAlias, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn extra_alias_is_not_an_error() {
        let topology = topology_with(vec![agent("default", "default", &[])]);
        let mut models = HashMap::new();
        models.insert(
            "default".to_string(),
            alias("anthropic", "claude-sonnet-4-6", "sk-foo"),
        );
        models.insert(
            "unused".to_string(),
            alias("anthropic", "claude-haiku", "sk-bar"),
        );
        let auth = auth_with(models);

        let mut errors = ConfigErrors::new();
        let resolved = resolve_aliases(&topology, &auth, &mut errors);

        assert!(errors.is_empty());
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].model.alias, "default");
    }

    #[test]
    fn multiple_agents_share_one_alias() {
        let topology = topology_with(vec![
            agent("a1", "default", &[]),
            agent("a2", "default", &["read"]),
        ]);
        let mut models = HashMap::new();
        models.insert(
            "default".to_string(),
            alias("anthropic", "claude-sonnet-4-6", "sk-foo"),
        );
        let auth = auth_with(models);

        let mut errors = ConfigErrors::new();
        let resolved = resolve_aliases(&topology, &auth, &mut errors);

        assert!(errors.is_empty());
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].name, "a1");
        assert_eq!(resolved[1].name, "a2");
        assert_eq!(resolved[0].model.alias, "default");
        assert_eq!(resolved[1].model.alias, "default");
        assert_eq!(resolved[0].model.model_name, resolved[1].model.model_name);
        assert_eq!(resolved[0].model.api_key, resolved[1].model.api_key);
    }

    #[test]
    fn multiple_agents_with_different_aliases() {
        let topology = topology_with(vec![agent("a1", "fast", &[]), agent("a2", "smart", &[])]);
        let mut models = HashMap::new();
        models.insert(
            "fast".to_string(),
            alias("anthropic", "claude-haiku", "sk-fast"),
        );
        models.insert(
            "smart".to_string(),
            alias("anthropic", "claude-opus", "sk-smart"),
        );
        let auth = auth_with(models);

        let mut errors = ConfigErrors::new();
        let resolved = resolve_aliases(&topology, &auth, &mut errors);

        assert!(errors.is_empty());
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].model.model_name, "claude-haiku");
        assert_eq!(resolved[0].model.api_key, "sk-fast");
        assert_eq!(resolved[1].model.model_name, "claude-opus");
        assert_eq!(resolved[1].model.api_key, "sk-smart");
    }

    #[test]
    fn mixed_one_resolves_one_dangles() -> Result<(), Box<dyn std::error::Error>> {
        let topology = topology_with(vec![
            agent("good", "default", &[]),
            agent("bad", "missing", &[]),
        ]);
        let mut models = HashMap::new();
        models.insert(
            "default".to_string(),
            alias("anthropic", "claude-sonnet-4-6", "sk-foo"),
        );
        let auth = auth_with(models);

        let mut errors = ConfigErrors::new();
        let resolved = resolve_aliases(&topology, &auth, &mut errors);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "good");
        assert_eq!(errors.len(), 1);
        match &errors.as_slice()[0] {
            ConfigError::DanglingAlias { alias } => assert_eq!(alias, "missing"),
            other => return Err(format!("expected DanglingAlias, got {other:?}").into()),
        }
        Ok(())
    }
}
