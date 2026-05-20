//! Configuration types for the two-file split (topology + auth).
//!
//! `TopologyConfig` mirrors the embedded `default.toml`: agents, prompts, and
//! tool declarations. `AuthConfig` mirrors the user-global `pristine-auth.toml`:
//! providers, model aliases, and credentials. `Config` is the inert,
//! fully-resolved value that the rest of the binary walks; it is intentionally
//! skeletal in this bead and is filled in by later phases.
//!
//! No file IO, env-var access, templating, or alias resolution lives here —
//! those layers are added in subsequent beads on top of these types.

pub mod auth;
pub mod error;
pub mod parse;
pub mod topology;

pub use auth::{AuthConfig, ModelAliasConfig, ProviderConfig, ProviderKind};
pub use error::{ConfigError, ConfigErrors};
pub use parse::{parse_auth, parse_topology, read_auth_file, read_topology_file};
pub use topology::{AgentConfig, ToolConfig, ToolKind, TopologyConfig};

/// Inert, fully-resolved configuration handed from `pristine_config::load(...)`
/// to `run_async`. Skeletal in B1 — later phases (C1 onwards) populate fields
/// such as resolved agents, resolved models, and credentials. Kept as a unit
/// struct so downstream beads have a stable type to target without locking in
/// premature field shape.
#[derive(Debug, Default, Clone)]
pub struct Config {}

impl Config {
    pub fn new() -> Self {
        Self::default()
    }
}
