//! TOML deserialization entry points for the two config files.
//!
//! The plain `parse_topology` / `parse_auth` functions perform a single
//! `toml::from_str` decode into the typed structs from `topology` and `auth`,
//! mapping every failure into a `ConfigError` value that carries the
//! originating file path for diagnostics. They do not run templating; callers
//! that need `{{ENV_VAR}}` substitution use the `_with_env` variants, which
//! parse to `toml::Value`, run the templating walker, then deserialize into
//! the typed struct. Templating errors are accumulated into a `ConfigErrors`
//! aggregate so the caller sees every missing env var at once.

use std::path::Path;

use crate::config::auth::AuthConfig;
use crate::config::error::{ConfigError, ConfigErrors};
use crate::config::template::{EnvSource, template_value};
use crate::config::topology::TopologyConfig;

/// Decode topology TOML from an in-memory string. `path` is used only to label
/// errors; nothing is read from disk.
pub fn parse_topology(input: &str, path: &Path) -> Result<TopologyConfig, ConfigError> {
    toml::from_str::<TopologyConfig>(input).map_err(|source| ConfigError::TomlParse {
        path: path.to_path_buf(),
        source,
    })
}

/// Decode auth TOML from an in-memory string. `path` is used only to label
/// errors; nothing is read from disk.
pub fn parse_auth(input: &str, path: &Path) -> Result<AuthConfig, ConfigError> {
    toml::from_str::<AuthConfig>(input).map_err(|source| ConfigError::TomlParse {
        path: path.to_path_buf(),
        source,
    })
}

/// Decode topology TOML with `{{ENV_VAR}}` templating applied. The input is
/// first parsed into a generic `toml::Value`, every string node is run through
/// the templating walker against `env`, and the patched tree is then decoded
/// into `TopologyConfig`. Every error (initial parse, missing env var, final
/// deserialization) is accumulated into the returned `ConfigErrors` so the
/// caller sees the complete set in one pass. The deserialization error after
/// templating lacks line/column spans because the parsed value no longer
/// carries the original byte ranges; the initial parse error retains its span.
pub fn parse_topology_with_env<E: EnvSource>(
    input: &str,
    path: &Path,
    env: &E,
) -> Result<TopologyConfig, ConfigErrors> {
    parse_with_env::<TopologyConfig, E>(input, path, env)
}

/// Decode auth TOML with `{{ENV_VAR}}` templating applied. See
/// `parse_topology_with_env` for the error-collection semantics.
pub fn parse_auth_with_env<E: EnvSource>(
    input: &str,
    path: &Path,
    env: &E,
) -> Result<AuthConfig, ConfigErrors> {
    parse_with_env::<AuthConfig, E>(input, path, env)
}

fn parse_with_env<T, E>(input: &str, path: &Path, env: &E) -> Result<T, ConfigErrors>
where
    T: serde::de::DeserializeOwned,
    E: EnvSource,
{
    let mut tree = match toml::from_str::<toml::Value>(input) {
        Ok(value) => value,
        Err(source) => {
            let mut errors = ConfigErrors::new();
            errors.push(ConfigError::TomlParse {
                path: path.to_path_buf(),
                source,
            });
            return Err(errors);
        }
    };
    let mut errors = ConfigErrors::new();
    template_value(&mut tree, env, &mut errors);
    if !errors.is_empty() {
        return Err(errors);
    }
    tree.try_into::<T>().map_err(|source| {
        let mut errors = ConfigErrors::new();
        errors.push(ConfigError::TomlParse {
            path: path.to_path_buf(),
            source,
        });
        errors
    })
}

/// Read a topology file from disk and decode it. File-read failures surface as
/// `ConfigError::IoError`; parse failures surface as `ConfigError::TomlParse`.
pub fn read_topology_file(path: &Path) -> Result<TopologyConfig, ConfigError> {
    let input = std::fs::read_to_string(path).map_err(|source| ConfigError::IoError {
        path: path.to_path_buf(),
        source,
    })?;
    parse_topology(&input, path)
}

/// Read an auth file from disk and decode it. File-read failures surface as
/// `ConfigError::IoError`; parse failures surface as `ConfigError::TomlParse`.
pub fn read_auth_file(path: &Path) -> Result<AuthConfig, ConfigError> {
    let input = std::fs::read_to_string(path).map_err(|source| ConfigError::IoError {
        path: path.to_path_buf(),
        source,
    })?;
    parse_auth(&input, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::auth::ProviderConfig;
    use crate::config::topology::ToolConfig;

    const TOPOLOGY_PATH: &str = "/virtual/topology.toml";
    const AUTH_PATH: &str = "/virtual/auth.toml";

    #[test]
    fn parse_topology_round_trips_valid_input() -> Result<(), Box<dyn std::error::Error>> {
        let input = r#"
[[agents]]
name = "default"
model = "default"
system_prompt = "you are pristine"
tools = ["read", "write", "exec_bash"]

[tools.read]
type = "builtin"
builtin = "read"

[tools.write]
type = "builtin"
builtin = "write"

[tools.exec_bash]
type = "builtin"
builtin = "exec_bash"
"#;
        let cfg = parse_topology(input, Path::new(TOPOLOGY_PATH)).expect("valid topology parses");
        assert_eq!(cfg.agents.len(), 1);
        let agent = &cfg.agents[0];
        assert_eq!(agent.name, "default");
        assert_eq!(agent.model, "default");
        assert_eq!(agent.system_prompt, "you are pristine");
        assert_eq!(agent.tools, vec!["read", "write", "exec_bash"]);
        assert_eq!(cfg.tools.len(), 3);
        match cfg.tools.get("read") {
            Some(ToolConfig::Builtin { builtin }) => assert_eq!(builtin, "read"),
            other => return Err(format!("expected builtin read tool, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn parse_topology_rejects_unknown_field_on_agent() -> Result<(), Box<dyn std::error::Error>> {
        let input = r#"
[[agents]]
name = "default"
model = "default"
system_prompt = "hi"
tools = []
rogue = "nope"
"#;
        let err =
            parse_topology(input, Path::new(TOPOLOGY_PATH)).expect_err("unknown field rejected");
        match err {
            ConfigError::TomlParse { path, source } => {
                assert_eq!(path, Path::new(TOPOLOGY_PATH));
                assert!(
                    source.to_string().contains("rogue"),
                    "underlying error mentions unknown field: {source}"
                );
            }
            other => return Err(format!("expected TomlParse, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn parse_topology_garbled_syntax_carries_span() -> Result<(), Box<dyn std::error::Error>> {
        let input = "this is not = = valid toml [[";
        let err =
            parse_topology(input, Path::new(TOPOLOGY_PATH)).expect_err("garbled toml rejected");
        match err {
            ConfigError::TomlParse { path, source } => {
                assert_eq!(path, Path::new(TOPOLOGY_PATH));
                assert!(
                    source.span().is_some(),
                    "garbled toml should expose a span: {source}"
                );
            }
            other => return Err(format!("expected TomlParse, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn parse_auth_round_trips_valid_input() -> Result<(), Box<dyn std::error::Error>> {
        let input = r#"
[providers.anthropic]
type = "anthropic"

[models.default]
provider = "anthropic"
model_name = "claude-sonnet-4-6"
api_key = "{{ANTHROPIC_API_KEY}}"
"#;
        let cfg = parse_auth(input, Path::new(AUTH_PATH)).expect("valid auth parses");
        assert_eq!(cfg.providers.len(), 1);
        match cfg.providers.get("anthropic") {
            Some(ProviderConfig::Anthropic { base_url }) => assert!(base_url.is_none()),
            None => return Err("expected anthropic provider".into()),
        }
        let alias = cfg.models.get("default").expect("default alias present");
        assert_eq!(alias.provider, "anthropic");
        assert_eq!(alias.model_name, "claude-sonnet-4-6");
        assert_eq!(alias.api_key, "{{ANTHROPIC_API_KEY}}");
        Ok(())
    }

    #[test]
    fn parse_auth_garbled_syntax_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let input = "[providers.anthropic\ntype = \"anthropic\"";
        let err = parse_auth(input, Path::new(AUTH_PATH)).expect_err("garbled auth rejected");
        match err {
            ConfigError::TomlParse { path, .. } => assert_eq!(path, Path::new(AUTH_PATH)),
            other => return Err(format!("expected TomlParse, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn read_topology_file_missing_path_returns_io_error() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir().expect("tempdir creation succeeds");
        let missing = dir.path().join("does-not-exist.toml");
        let err = read_topology_file(&missing).expect_err("missing file returns IoError");
        match err {
            ConfigError::IoError { path, source } => {
                assert_eq!(path, missing);
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => return Err(format!("expected IoError, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn read_topology_file_round_trips_valid_file() {
        let input = r#"
[[agents]]
name = "default"
model = "default"
system_prompt = "you are pristine"
tools = []
"#;
        let dir = tempfile::tempdir().expect("tempdir creation succeeds");
        let path = dir.path().join("topology.toml");
        std::fs::write(&path, input).expect("write tempfile");
        let cfg = read_topology_file(&path).expect("valid file parses");
        assert_eq!(cfg.agents.len(), 1);
        assert_eq!(cfg.agents[0].name, "default");
    }

    use crate::test_support::MapEnv;

    #[test]
    fn parse_auth_with_env_substitutes_api_key() {
        let input = r#"
[providers.anthropic]
type = "anthropic"

[models.default]
provider = "anthropic"
model_name = "claude-sonnet-4-6"
api_key = "{{ANTHROPIC_API_KEY}}"
"#;
        let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-foo")]);
        let cfg =
            parse_auth_with_env(input, Path::new(AUTH_PATH), &env).expect("templated auth parses");
        let alias = cfg.models.get("default").expect("default alias present");
        assert_eq!(alias.api_key, "sk-foo");
    }

    #[test]
    fn parse_auth_with_env_missing_var_collects_error() -> Result<(), Box<dyn std::error::Error>> {
        let input = r#"
[providers.anthropic]
type = "anthropic"

[models.default]
provider = "anthropic"
model_name = "claude-sonnet-4-6"
api_key = "{{ANTHROPIC_API_KEY}}"
"#;
        let env = MapEnv::default();
        let errors = parse_auth_with_env(input, Path::new(AUTH_PATH), &env)
            .expect_err("missing env var surfaces as error");
        assert_eq!(errors.len(), 1);
        match &errors.as_slice()[0] {
            ConfigError::UnknownEnvVar { name, location } => {
                assert_eq!(name, "ANTHROPIC_API_KEY");
                assert!(
                    location.contains("models")
                        && location.contains("default")
                        && location.contains("api_key"),
                    "location {location:?} should reference the api_key path"
                );
            }
            other => return Err(format!("expected UnknownEnvVar, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn parse_topology_with_env_substitutes_system_prompt() {
        let input = r#"
[[agents]]
name = "default"
model = "default"
system_prompt = "you are {{ROLE}}"
tools = []
"#;
        let env = MapEnv::new([("ROLE", "pristine")]);
        let cfg = parse_topology_with_env(input, Path::new(TOPOLOGY_PATH), &env)
            .expect("templated topology parses");
        assert_eq!(cfg.agents[0].system_prompt, "you are pristine");
    }

    #[test]
    fn parse_topology_with_env_initial_parse_error_short_circuits()
    -> Result<(), Box<dyn std::error::Error>> {
        let input = "this is not = = valid toml [[";
        let env = MapEnv::default();
        let errors = parse_topology_with_env(input, Path::new(TOPOLOGY_PATH), &env)
            .expect_err("garbled toml rejected");
        assert_eq!(errors.len(), 1);
        match &errors.as_slice()[0] {
            ConfigError::TomlParse { path, .. } => {
                assert_eq!(path, Path::new(TOPOLOGY_PATH));
            }
            other => return Err(format!("expected TomlParse, got {other:?}").into()),
        }
        Ok(())
    }
}
