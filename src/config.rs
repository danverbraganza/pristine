//! Two-file TOML configuration model: a topology file (agents, tools, prompts)
//! and an auth file (providers, model aliases, credentials).
//!
//! `{{ENV_VAR}}` templating walks the parsed `toml::Value` tree and substitutes
//! placeholders using an `EnvSource`. Alias resolution looks up each agent's
//! `model = "X"` against `auth.models[X]`. Tool-reference validation requires
//! every entry of an agent's `tools = [...]` to be a key declared in
//! `topology.tools`.
//!
//! `assemble_config<E: EnvSource>` is the orchestrator: parse, template,
//! resolve, validate. Errors accumulate into `ConfigErrors`; the call returns
//! either `Ok(Config)` or `Err(ConfigErrors)`.

pub mod auth;
pub mod error;
pub mod parse;
pub mod resolve;
pub mod template;
pub mod topology;
pub mod validate;

use std::collections::HashMap;
use std::path::Path;

pub use auth::{AuthConfig, ModelAliasConfig, ProviderConfig};
pub use error::{ConfigError, ConfigErrors};
pub use parse::{
    parse_auth, parse_auth_with_env, parse_topology, parse_topology_with_env, read_auth_file,
    read_topology_file,
};
pub use resolve::{ResolvedAgent, ResolvedModel, resolve_aliases};
pub use template::{EnvSource, ProcessEnv, template_value};
pub use topology::{AgentConfig, ToolConfig, TopologyConfig};
pub use validate::validate_tool_refs;

/// Inert, fully-resolved configuration handed from `pristine_config::load(...)`
/// to `run_async`. Agents have their model aliases pre-resolved into a
/// `ResolvedModel` so the binary can walk this value and issue
/// `HarnessBuilder` calls without re-consulting the auth file. The `tools` and
/// `providers` maps are cloned from the underlying topology and auth values so
/// downstream callers do not retain a borrow on the originals.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub agents: Vec<ResolvedAgent>,
    pub tools: HashMap<String, ToolConfig>,
    pub providers: HashMap<String, ProviderConfig>,
}

impl Config {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Top-level config assembly: parse + template the two TOML inputs, run every
/// resolution / validation pass that has usable inputs, and return either a
/// fully-resolved `Config` or every error collected across both files in one
/// `ConfigErrors`.
///
/// "Parse don't validate" boundary: a TOML parse failure on one file is fatal
/// for that file (no usable struct, so dependent passes are skipped) but does
/// NOT short-circuit the other file's parse, nor any independent passes. The
/// caller sees the maximal collection of errors that could be discovered in
/// one walk.
pub fn assemble_config<E: EnvSource>(
    topology_input: &str,
    topology_path: &Path,
    auth_input: &str,
    auth_path: &Path,
    env: &E,
) -> Result<Config, ConfigErrors> {
    let mut errors = ConfigErrors::new();

    let topology = match parse_topology_with_env(topology_input, topology_path, env) {
        Ok(value) => Some(value),
        Err(parse_errors) => {
            errors.extend(parse_errors);
            None
        }
    };

    let auth = match parse_auth_with_env(auth_input, auth_path, env) {
        Ok(value) => Some(value),
        Err(parse_errors) => {
            errors.extend(parse_errors);
            None
        }
    };

    if let Some(ref topology) = topology {
        validate_tool_refs(topology, &mut errors);
    }

    let resolved_agents = match (topology.as_ref(), auth.as_ref()) {
        (Some(topology), Some(auth)) => resolve_aliases(topology, auth, &mut errors),
        _ => Vec::new(),
    };

    if !errors.is_empty() {
        return Err(errors);
    }

    // Both files parsed cleanly when we reach this point: any topology / auth
    // parse failure would have appended to `errors` and tripped the early
    // return above. Unwrapping the `Option`s here is the natural shape, but
    // we restate it as a structural match to keep `unwrap()` out of
    // production code.
    let (topology, auth) = match (topology, auth) {
        (Some(t), Some(a)) => (t, a),
        _ => return Err(errors),
    };

    Ok(Config {
        agents: resolved_agents,
        tools: topology.tools.clone(),
        providers: auth.providers.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;

    const TOPOLOGY_PATH: &str = "/virtual/topology.toml";
    const AUTH_PATH: &str = "/virtual/auth.toml";

    /// In-memory `EnvSource` for deterministic assembly tests.
    #[derive(Default)]
    struct MapEnv(StdHashMap<String, String>);

    impl MapEnv {
        fn new<const N: usize>(entries: [(&str, &str); N]) -> Self {
            let mut map = StdHashMap::new();
            for (k, v) in entries {
                map.insert(k.to_string(), v.to_string());
            }
            Self(map)
        }
    }

    impl EnvSource for MapEnv {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    fn valid_topology() -> &'static str {
        r#"
[[agents]]
name = "default"
model = "default"
system_prompt = "you are pristine"
tools = ["read", "write"]

[tools.read]
type = "builtin"
builtin = "read"

[tools.write]
type = "builtin"
builtin = "write"
"#
    }

    fn valid_auth() -> &'static str {
        r#"
[providers.anthropic]
type = "anthropic"

[models.default]
provider = "anthropic"
model_name = "claude-sonnet-4-6"
api_key = "{{ANTHROPIC_API_KEY}}"
"#
    }

    #[test]
    fn assemble_config_happy_path() {
        let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-foo")]);
        let config = assemble_config(
            valid_topology(),
            Path::new(TOPOLOGY_PATH),
            valid_auth(),
            Path::new(AUTH_PATH),
            &env,
        )
        .expect("happy path assembles");

        assert_eq!(config.agents.len(), 1);
        let agent = &config.agents[0];
        assert_eq!(agent.name, "default");
        assert_eq!(agent.system_prompt, "you are pristine");
        assert_eq!(agent.tools, vec!["read".to_string(), "write".to_string()]);
        assert_eq!(agent.model.alias, "default");
        assert_eq!(agent.model.provider_name, "anthropic");
        assert_eq!(agent.model.model_name, "claude-sonnet-4-6");
        assert_eq!(agent.model.api_key, "sk-foo");
        assert_eq!(config.tools.len(), 2);
        assert!(config.tools.contains_key("read"));
        assert!(config.tools.contains_key("write"));
        assert_eq!(config.providers.len(), 1);
        assert!(config.providers.contains_key("anthropic"));
    }

    #[test]
    fn assemble_config_topology_parse_error_collects_auth_errors_too() {
        let broken_topology = "this is not = = valid toml [[";
        let env = MapEnv::default();
        let errors = assemble_config(
            broken_topology,
            Path::new(TOPOLOGY_PATH),
            valid_auth(),
            Path::new(AUTH_PATH),
            &env,
        )
        .expect_err("broken topology + missing env should error");

        let mut saw_toml_parse_on_topology = false;
        let mut saw_unknown_env_on_auth = false;
        for err in errors.as_slice() {
            match err {
                ConfigError::TomlParse { path, .. } if path == Path::new(TOPOLOGY_PATH) => {
                    saw_toml_parse_on_topology = true;
                }
                ConfigError::UnknownEnvVar { name, .. } if name == "ANTHROPIC_API_KEY" => {
                    saw_unknown_env_on_auth = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_toml_parse_on_topology,
            "expected a TomlParse error for the topology file; got {errors}"
        );
        assert!(
            saw_unknown_env_on_auth,
            "expected an UnknownEnvVar error for ANTHROPIC_API_KEY; got {errors}"
        );
    }

    #[test]
    fn assemble_config_dangling_alias_and_undeclared_tool_collected_together() {
        let topology = r#"
[[agents]]
name = "default"
model = "nope"
system_prompt = "hi"
tools = ["nonsense"]
"#;
        let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-foo")]);
        let errors = assemble_config(
            topology,
            Path::new(TOPOLOGY_PATH),
            valid_auth(),
            Path::new(AUTH_PATH),
            &env,
        )
        .expect_err("dangling alias + undeclared tool should error");

        let mut saw_dangling = false;
        let mut saw_undeclared = false;
        for err in errors.as_slice() {
            match err {
                ConfigError::DanglingAlias { alias } if alias == "nope" => {
                    saw_dangling = true;
                }
                ConfigError::UndeclaredTool { agent, tool }
                    if agent == "default" && tool == "nonsense" =>
                {
                    saw_undeclared = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_dangling,
            "expected DanglingAlias for 'nope'; got {errors}"
        );
        assert!(
            saw_undeclared,
            "expected UndeclaredTool for agent 'default', tool 'nonsense'; got {errors}"
        );
    }

    #[test]
    fn assemble_config_all_pass_returns_config() {
        let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-bar")]);
        let config = assemble_config(
            valid_topology(),
            Path::new(TOPOLOGY_PATH),
            valid_auth(),
            Path::new(AUTH_PATH),
            &env,
        )
        .expect("all-pass produces a Config");

        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.tools.len(), 2);
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.agents[0].model.api_key, "sk-bar");
    }
}
