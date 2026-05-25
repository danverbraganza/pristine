//! Auth and topology file path discovery.
//!
//! Pure path math: resolves CLI overrides against an injected `HomeSource`,
//! expanding a leading `~` where applicable. No file IO and no auto-write —
//! `autowrite::ensure_auth_file` handles missing-file materialization and
//! `config::load_with` orchestrates the full read + parse + assemble pipeline.
//!
//! The `HomeSource` trait mirrors `EnvSource` from `template.rs`: production
//! code uses `ProcessHome` which reads `HOME` from the process environment;
//! tests substitute a `MockHome` to keep resolution deterministic.

use std::path::{Path, PathBuf};

use crate::config::error::ConfigError;

/// Abstraction over the user's home directory lookup. The production impl
/// reads `HOME` from the process environment; tests use an in-memory stand-in.
pub trait HomeSource {
    fn home_dir(&self) -> Option<PathBuf>;
}

/// Default `HomeSource` backed by `std::env::var_os("HOME")`.
pub struct ProcessHome;

impl HomeSource for ProcessHome {
    fn home_dir(&self) -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Default basename of the auth file under the user's home directory.
const AUTH_FILE_NAME: &str = "pristine-auth.toml";

/// Resolve the auth file path from an optional CLI override and a `HomeSource`.
///
/// - `Some(p)`: expand a leading `~` against `home`, then return the result.
/// - `None`: default to `<home>/pristine-auth.toml`; missing `HOME` becomes
///   `ConfigError::MissingHome`.
pub fn resolve_auth_path<H: HomeSource>(
    cli_override: Option<&Path>,
    home: &H,
) -> Result<PathBuf, ConfigError> {
    match cli_override {
        Some(p) => expand_tilde(p, home),
        None => {
            let home_dir = home.home_dir().ok_or(ConfigError::MissingHome)?;
            Ok(home_dir.join(AUTH_FILE_NAME))
        }
    }
}

/// Resolve the topology file path from an optional CLI override and a
/// `HomeSource`. Unlike auth, topology has no default file path: when no
/// override is supplied the caller falls back to the embedded `default.toml`.
///
/// - `Some(p)`: expand a leading `~` against `home`, then return
///   `Ok(Some(expanded))`.
/// - `None`: `Ok(None)`.
pub fn resolve_topology_path<H: HomeSource>(
    cli_override: Option<&Path>,
    home: &H,
) -> Result<Option<PathBuf>, ConfigError> {
    match cli_override {
        Some(p) => Ok(Some(expand_tilde(p, home)?)),
        None => Ok(None),
    }
}

/// Replace a leading `~` in `path` with the user's home directory. Paths that
/// do not start with `~` are returned verbatim. A path that needs expansion
/// but whose `HomeSource` returns `None` becomes `ConfigError::MissingHome`.
fn expand_tilde<H: HomeSource>(path: &Path, home: &H) -> Result<PathBuf, ConfigError> {
    let Some(s) = path.to_str() else {
        return Ok(path.to_path_buf());
    };
    if s == "~" {
        return home.home_dir().ok_or(ConfigError::MissingHome);
    }
    if let Some(rest) = s.strip_prefix("~/") {
        let home_dir = home.home_dir().ok_or(ConfigError::MissingHome)?;
        return Ok(home_dir.join(rest));
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockHome;

    #[test]
    fn resolve_auth_default_path_uses_home() {
        let home = MockHome::some("/tmp/user-home");
        let resolved = resolve_auth_path(None, &home).expect("default path resolves");
        assert_eq!(resolved, PathBuf::from("/tmp/user-home/pristine-auth.toml"));
    }

    #[test]
    fn resolve_auth_missing_home_returns_missing_home_error() {
        let home = MockHome::none();
        let err = resolve_auth_path(None, &home).expect_err("missing home should error");
        assert!(matches!(err, ConfigError::MissingHome));
    }

    #[test]
    fn resolve_auth_cli_override_used_verbatim() {
        let home = MockHome::some("/tmp/irrelevant");
        let override_path = PathBuf::from("/etc/pristine.toml");
        let resolved =
            resolve_auth_path(Some(&override_path), &home).expect("override path resolves");
        assert_eq!(resolved, PathBuf::from("/etc/pristine.toml"));
    }

    #[test]
    fn resolve_auth_cli_override_with_tilde_expands() {
        let home = MockHome::some("/tmp/h");
        let override_path = PathBuf::from("~/conf.toml");
        let resolved =
            resolve_auth_path(Some(&override_path), &home).expect("tilde override resolves");
        assert_eq!(resolved, PathBuf::from("/tmp/h/conf.toml"));
    }

    #[test]
    fn resolve_auth_cli_override_with_tilde_missing_home_errors() {
        let home = MockHome::none();
        let override_path = PathBuf::from("~/conf.toml");
        let err = resolve_auth_path(Some(&override_path), &home)
            .expect_err("tilde override without home should error");
        assert!(matches!(err, ConfigError::MissingHome));
    }

    #[test]
    fn resolve_topology_none_returns_none() {
        let home = MockHome::some("/tmp/h");
        let resolved = resolve_topology_path(None, &home).expect("None resolves");
        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_topology_cli_override_returns_path() {
        let home = MockHome::some("/tmp/h");
        let override_path = PathBuf::from("/etc/topo.toml");
        let resolved =
            resolve_topology_path(Some(&override_path), &home).expect("topology override resolves");
        assert_eq!(resolved, Some(PathBuf::from("/etc/topo.toml")));
    }

    #[test]
    fn resolve_topology_cli_override_with_tilde_expands() {
        let home = MockHome::some("/tmp/h");
        let override_path = PathBuf::from("~/topo.toml");
        let resolved = resolve_topology_path(Some(&override_path), &home)
            .expect("tilde topology override resolves");
        assert_eq!(resolved, Some(PathBuf::from("/tmp/h/topo.toml")));
    }

    #[test]
    fn expand_tilde_plain_path_unchanged() {
        let home = MockHome::some("/tmp/h");
        let resolved =
            expand_tilde(Path::new("/abs/no/tilde.toml"), &home).expect("plain path resolves");
        assert_eq!(resolved, PathBuf::from("/abs/no/tilde.toml"));
    }

    #[test]
    fn expand_tilde_alone_returns_home_directly() {
        let home = MockHome::some("/tmp/home");
        let resolved = expand_tilde(Path::new("~"), &home).expect("~ alone resolves");
        assert_eq!(resolved, PathBuf::from("/tmp/home"));
    }
}
