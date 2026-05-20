//! Configuration error types.
//!
//! Hand-rolled `Display` + `Error` impls follow the pattern in `crate::tool`
//! to keep the dependency surface narrow (no `thiserror`). `ConfigErrors`
//! aggregates failures collected during a single load so the user sees every
//! parseable problem at once instead of a one-at-a-time peel.

use std::path::PathBuf;

#[derive(Debug)]
pub enum ConfigError {
    /// A TOML file failed to parse. The `path` identifies which file; the
    /// `source` carries the underlying `toml::de::Error` with line/column.
    TomlParse {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// An `{{ENV_VAR}}` placeholder referenced a variable that is not set in
    /// the current process environment. `location` is a free-form description
    /// of where in the document the placeholder appeared (e.g. a TOML key
    /// path).
    UnknownEnvVar { name: String, location: String },
    /// A topology agent referenced a `model = "X"` whose alias is absent from
    /// the auth file.
    DanglingAlias { alias: String },
    /// The auth file declared a `[models.X]` alias that no topology agent
    /// references.
    ExtraneousAlias { alias: String },
    /// An agent listed a tool name that is not declared in the topology's
    /// `[tools]` table.
    UndeclaredTool { agent: String, tool: String },
    /// An agent listed the same tool name more than once in its `tools = [...]`
    /// array.
    DuplicateToolRef { agent: String, tool: String },
    /// A model alias named a provider that is absent from the auth file.
    UnknownProvider { name: String },
    /// A filesystem read or write failed. The `path` identifies which file;
    /// the `source` carries the underlying `std::io::Error`.
    IoError {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The user's home directory could not be determined, so the default auth
    /// file path could not be expanded.
    MissingHome,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::TomlParse { path, source } => {
                write!(f, "TOML parse error in {}: {source}", path.display())
            }
            ConfigError::UnknownEnvVar { name, location } => {
                write!(
                    f,
                    "environment variable {name} (referenced at {location}) is not set"
                )
            }
            ConfigError::DanglingAlias { alias } => {
                write!(
                    f,
                    "model alias '{alias}' referenced by topology but absent from auth file"
                )
            }
            ConfigError::ExtraneousAlias { alias } => {
                write!(
                    f,
                    "model alias '{alias}' declared in auth file but unused by topology"
                )
            }
            ConfigError::UndeclaredTool { agent, tool } => {
                write!(
                    f,
                    "agent '{agent}' references tool '{tool}' which is not declared in [tools]"
                )
            }
            ConfigError::DuplicateToolRef { agent, tool } => {
                write!(f, "agent '{agent}' lists tool '{tool}' more than once")
            }
            ConfigError::UnknownProvider { name } => {
                write!(f, "unknown provider '{name}'")
            }
            ConfigError::IoError { path, source } => {
                write!(f, "I/O error on {}: {source}", path.display())
            }
            ConfigError::MissingHome => {
                write!(f, "could not determine user home directory")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::TomlParse { source, .. } => Some(source),
            ConfigError::IoError { source, .. } => Some(source),
            ConfigError::UnknownEnvVar { .. }
            | ConfigError::DanglingAlias { .. }
            | ConfigError::ExtraneousAlias { .. }
            | ConfigError::UndeclaredTool { .. }
            | ConfigError::DuplicateToolRef { .. }
            | ConfigError::UnknownProvider { .. }
            | ConfigError::MissingHome => None,
        }
    }
}

/// Aggregate of every `ConfigError` produced during one `load(...)` call.
/// Returned by Phase C3's parse-don't-validate boundary; consumers can either
/// inspect the contained vector or rely on the `Display` impl, which walks
/// every contained error in registration order.
#[derive(Debug, Default)]
pub struct ConfigErrors(Vec<ConfigError>);

impl ConfigErrors {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, error: ConfigError) {
        self.0.push(error);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn into_inner(self) -> Vec<ConfigError> {
        self.0
    }

    pub fn as_slice(&self) -> &[ConfigError] {
        &self.0
    }
}

impl std::fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            return write!(f, "no configuration errors");
        }
        writeln!(f, "{} configuration error(s):", self.0.len())?;
        for (idx, err) in self.0.iter().enumerate() {
            writeln!(f, "  {}. {err}", idx + 1)?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigErrors {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_std_error<T: std::error::Error + Send + Sync + 'static>() {}

    #[test]
    fn config_error_is_standard_error_trait_object() {
        assert_std_error::<ConfigError>();
    }

    #[test]
    fn config_errors_is_standard_error_trait_object() {
        assert_std_error::<ConfigErrors>();
    }

    #[test]
    fn empty_aggregate_is_empty() {
        let errors = ConfigErrors::new();
        assert!(errors.is_empty());
        assert_eq!(errors.len(), 0);
    }

    #[test]
    fn push_then_into_inner_round_trips() {
        let mut errors = ConfigErrors::new();
        errors.push(ConfigError::DanglingAlias {
            alias: "default".to_string(),
        });
        errors.push(ConfigError::MissingHome);
        assert_eq!(errors.len(), 2);
        let inner = errors.into_inner();
        assert_eq!(inner.len(), 2);
    }
}
