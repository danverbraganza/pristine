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
pub mod autowrite;
pub mod discover;
pub mod error;
pub mod parse;
pub mod resolve;
pub mod template;
pub mod topology;
pub mod validate;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use auth::{AuthConfig, ModelAliasConfig, ProviderConfig};
pub use autowrite::ensure_auth_file;
pub use discover::{HomeSource, ProcessHome, resolve_auth_path, resolve_topology_path};
pub use error::{ConfigError, ConfigErrors};
pub use parse::{
    parse_auth, parse_auth_with_env, parse_topology, parse_topology_with_env, read_auth_file,
    read_topology_file,
};
pub use resolve::{ResolvedAgent, ResolvedModel, resolve_aliases};
pub use template::{EnvSource, ProcessEnv, template_value};
pub use topology::{AgentConfig, ToolConfig, TopologyConfig};
pub use validate::validate_tool_refs;

/// Canonical default topology shipped with pristine: the coding-assistant
/// prompt plus the five built-in tools, embedded at compile time and used as
/// the fallback when no `-c/--config` override is supplied.
const DEFAULT_TOPOLOGY: &str = include_str!("../default.toml");

/// Synthetic path label attached to TOML errors raised against the embedded
/// `default.toml`. Has no on-disk counterpart.
const EMBEDDED_DEFAULT_LABEL: &str = "<embedded default.toml>";

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

/// CLI-supplied inputs that select the topology and auth files for one call to
/// [`load`]. Both fields are optional overrides; their `None` shapes select the
/// embedded `default.toml` (for `config`) and `<home>/pristine-auth.toml` (for
/// `auth`).
///
/// Borrow-based to avoid `PathBuf` clones at the CLI boundary; the binary owns
/// the parsed clap values and passes them by reference.
#[derive(Debug, Clone, Copy)]
pub struct LoadArgs<'a> {
    /// CLI `-c/--config` override. `None` selects the embedded `default.toml`.
    pub config: Option<&'a Path>,
    /// CLI `--auth` override. `None` selects `<home>/pristine-auth.toml`.
    pub auth: Option<&'a Path>,
}

impl<'a> LoadArgs<'a> {
    /// Construct a `LoadArgs` with both overrides absent.
    pub fn new() -> Self {
        Self {
            config: None,
            auth: None,
        }
    }
}

impl Default for LoadArgs<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Top-level config orchestration. Resolves both file paths against the
/// supplied `HomeSource`, auto-writes the auth file if it is missing, reads
/// both files (falling back to the embedded `default.toml` when no override is
/// supplied), and hands the contents to [`assemble_config`].
///
/// "Parse don't validate": collects every recoverable failure into a
/// [`ConfigErrors`] before returning. Hard short-circuits are limited to cases
/// where no input is available at all — no auth path, no readable auth
/// content, no topology path.
pub fn load_with<H: HomeSource, E: EnvSource>(
    args: LoadArgs<'_>,
    home: &H,
    env: &E,
) -> Result<Config, ConfigErrors> {
    let mut errors = ConfigErrors::new();

    let auth_path = match resolve_auth_path(args.auth, home) {
        Ok(p) => p,
        Err(err) => {
            errors.push(err);
            return Err(errors);
        }
    };

    if let Err(err) = ensure_auth_file(&auth_path) {
        errors.push(err);
    }

    let auth_input = match std::fs::read_to_string(&auth_path) {
        Ok(s) => s,
        Err(source) => {
            errors.push(ConfigError::IoError {
                path: auth_path.clone(),
                source,
            });
            return Err(errors);
        }
    };

    let topology_path_override = match resolve_topology_path(args.config, home) {
        Ok(p) => p,
        Err(err) => {
            errors.push(err);
            return Err(errors);
        }
    };

    let (topology_input, topology_path) = match topology_path_override {
        Some(p) => match std::fs::read_to_string(&p) {
            Ok(s) => (s, p),
            Err(source) => {
                errors.push(ConfigError::IoError {
                    path: p.clone(),
                    source,
                });
                (String::new(), p)
            }
        },
        None => (
            DEFAULT_TOPOLOGY.to_string(),
            PathBuf::from(EMBEDDED_DEFAULT_LABEL),
        ),
    };

    match assemble_config(
        &topology_input,
        &topology_path,
        &auth_input,
        &auth_path,
        env,
    ) {
        Ok(config) if errors.is_empty() => Ok(config),
        Ok(_) => Err(errors),
        Err(assemble_errors) => {
            errors.extend(assemble_errors);
            Err(errors)
        }
    }
}

/// Production entry point: forwards to [`load_with`] using the real process
/// environment for `HOME` and env-var lookups.
pub fn load(args: LoadArgs<'_>) -> Result<Config, ConfigErrors> {
    load_with(args, &ProcessHome, &ProcessEnv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MapEnv;

    const TOPOLOGY_PATH: &str = "/virtual/topology.toml";
    const AUTH_PATH: &str = "/virtual/auth.toml";

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

    /// In-memory `HomeSource` for deterministic `load_with` tests.
    struct MockHome(Option<PathBuf>);

    impl MockHome {
        fn some(path: PathBuf) -> Self {
            Self(Some(path))
        }

        fn none() -> Self {
            Self(None)
        }
    }

    impl HomeSource for MockHome {
        fn home_dir(&self) -> Option<PathBuf> {
            self.0.clone()
        }
    }

    #[test]
    fn load_with_succeeds_for_valid_paths_and_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let topology_path = dir.path().join("topology.toml");
        let auth_path = dir.path().join("auth.toml");
        std::fs::write(&topology_path, valid_topology()).expect("write topology");
        std::fs::write(&auth_path, valid_auth()).expect("write auth");

        let home = MockHome::some(dir.path().to_path_buf());
        let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-load")]);
        let args = LoadArgs {
            config: Some(&topology_path),
            auth: Some(&auth_path),
        };
        let config = load_with(args, &home, &env).expect("load_with succeeds");

        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.agents[0].name, "default");
        assert_eq!(config.agents[0].model.api_key, "sk-load");
        assert_eq!(config.tools.len(), 2);
        assert_eq!(config.providers.len(), 1);
    }

    #[test]
    fn load_with_embedded_default_topology_when_no_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.toml");
        std::fs::write(&auth_path, valid_auth()).expect("write auth");

        let home = MockHome::some(dir.path().to_path_buf());
        let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-embedded")]);
        let args = LoadArgs {
            config: None,
            auth: Some(&auth_path),
        };
        let config = load_with(args, &home, &env).expect("embedded default loads");

        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.agents[0].name, "default");
        assert_eq!(config.agents[0].model.alias, "default");
    }

    #[test]
    fn load_with_auto_writes_missing_auth_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let topology_path = dir.path().join("topology.toml");
        std::fs::write(&topology_path, valid_topology()).expect("write topology");
        // Auth path under a nested subdirectory that does not exist; the
        // auto-write step must create it.
        let auth_path = dir.path().join("nested").join("pristine-auth.toml");
        assert!(!auth_path.exists(), "precondition: auth file absent");

        let home = MockHome::some(dir.path().to_path_buf());
        let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-autowrite")]);
        let args = LoadArgs {
            config: Some(&topology_path),
            auth: Some(&auth_path),
        };
        let config = load_with(args, &home, &env).expect("auto-write + load succeeds");

        assert!(
            auth_path.is_file(),
            "ensure_auth_file should have written the auth file"
        );
        let written = std::fs::read_to_string(&auth_path).expect("read back auth file");
        assert!(
            written.contains("[providers.anthropic]"),
            "auto-written file must contain the anthropic provider section: {written:?}"
        );
        // The template's `[models.default]` survives load_with because
        // ANTHROPIC_API_KEY was set in the env.
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.agents[0].model.api_key, "sk-autowrite");
    }

    #[test]
    fn embedded_default_topology_has_five_builtin_tools() {
        let topology: TopologyConfig =
            toml::from_str(DEFAULT_TOPOLOGY).expect("embedded default.toml parses");

        let mut keys: Vec<&str> = topology.tools.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["edit", "exec_bash", "insert", "read", "write"]);

        for expected in ["read", "write", "edit", "insert", "exec_bash"] {
            let tool = topology
                .tools
                .get(expected)
                .unwrap_or_else(|| panic!("embedded topology declares `{expected}` tool"));
            match tool {
                ToolConfig::Builtin { builtin } => assert_eq!(
                    builtin, expected,
                    "tool `{expected}` registers built-in named `{expected}`"
                ),
            }
        }
    }

    #[test]
    fn embedded_default_topology_has_default_agent_with_five_tools() {
        let topology: TopologyConfig =
            toml::from_str(DEFAULT_TOPOLOGY).expect("embedded default.toml parses");

        assert_eq!(
            topology.agents.len(),
            1,
            "embedded topology has exactly one agent"
        );
        let agent = &topology.agents[0];
        assert_eq!(agent.name, "default");
        assert_eq!(agent.model, "default");
        assert_eq!(
            agent.tools,
            vec![
                "read".to_string(),
                "write".to_string(),
                "edit".to_string(),
                "insert".to_string(),
                "exec_bash".to_string(),
            ]
        );
        assert!(
            agent.system_prompt.len() > 100,
            "system prompt is a real coding-assistant prompt, not a placeholder \
             (got {} chars)",
            agent.system_prompt.len()
        );
    }

    #[test]
    fn load_with_missing_home_when_default_auth_path_used() {
        let home = MockHome::none();
        let env = MapEnv::default();
        let args = LoadArgs::new();
        let errors =
            load_with(args, &home, &env).expect_err("missing home with default auth fails");
        let mut saw_missing_home = false;
        for err in errors.as_slice() {
            if matches!(err, ConfigError::MissingHome) {
                saw_missing_home = true;
            }
        }
        assert!(
            saw_missing_home,
            "expected ConfigError::MissingHome; got {errors}"
        );
    }
}
