//! TOML deserialization entry points for the two config files.
//!
//! These functions perform plain `toml::from_str` decoding into the typed
//! structs from `topology` and `auth`, mapping every failure into a
//! `ConfigError` value that carries the originating file path for diagnostics.
//! No templating, env-var access, or alias resolution happens here — those
//! layers are added in later beads on top of these entry points.

use std::path::Path;

use crate::config::auth::AuthConfig;
use crate::config::error::ConfigError;
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
    fn parse_topology_round_trips_valid_input() {
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
            other => panic!("expected builtin read tool, got {other:?}"),
        }
    }

    #[test]
    fn parse_topology_rejects_unknown_field_on_agent() {
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
            other => panic!("expected TomlParse, got {other:?}"),
        }
    }

    #[test]
    fn parse_topology_garbled_syntax_carries_span() {
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
            other => panic!("expected TomlParse, got {other:?}"),
        }
    }

    #[test]
    fn parse_auth_round_trips_valid_input() {
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
            None => panic!("expected anthropic provider"),
        }
        let alias = cfg.models.get("default").expect("default alias present");
        assert_eq!(alias.provider, "anthropic");
        assert_eq!(alias.model_name, "claude-sonnet-4-6");
        assert_eq!(alias.api_key, "{{ANTHROPIC_API_KEY}}");
    }

    #[test]
    fn parse_auth_garbled_syntax_rejected() {
        let input = "[providers.anthropic\ntype = \"anthropic\"";
        let err = parse_auth(input, Path::new(AUTH_PATH)).expect_err("garbled auth rejected");
        match err {
            ConfigError::TomlParse { path, .. } => assert_eq!(path, Path::new(AUTH_PATH)),
            other => panic!("expected TomlParse, got {other:?}"),
        }
    }

    #[test]
    fn read_topology_file_missing_path_returns_io_error() {
        let dir = tempfile::tempdir().expect("tempdir creation succeeds");
        let missing = dir.path().join("does-not-exist.toml");
        let err = read_topology_file(&missing).expect_err("missing file returns IoError");
        match err {
            ConfigError::IoError { path, source } => {
                assert_eq!(path, missing);
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected IoError, got {other:?}"),
        }
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
}
