//! Configuration types for the two-file split (topology + auth).
//!
//! `TopologyConfig` mirrors the embedded `default.toml`: agents, prompts, and
//! tool declarations. `AuthConfig` mirrors the user-global `pristine-auth.toml`:
//! providers, model aliases, and credentials. `Config` is the inert,
//! fully-resolved value that the rest of the binary walks: a list of resolved
//! agents (alias -> provider/model/api_key already looked up) plus the tool
//! and provider tables it needs at HarnessBuilder time.
//!
//! No file IO, env-var access, or templating lives here — those layers are
//! added in subsequent beads on top of these types.

pub mod auth;
pub mod error;
pub mod parse;
pub mod resolve;
pub mod template;
pub mod topology;
pub mod validate;

use std::collections::HashMap;

pub use auth::{AuthConfig, ModelAliasConfig, ProviderConfig, ProviderKind};
pub use error::{ConfigError, ConfigErrors};
pub use parse::{
    parse_auth, parse_auth_with_env, parse_topology, parse_topology_with_env, read_auth_file,
    read_topology_file,
};
pub use resolve::{ResolvedAgent, ResolvedModel, resolve_aliases};
pub use template::{EnvSource, ProcessEnv, template_value};
pub use topology::{AgentConfig, ToolConfig, ToolKind, TopologyConfig};
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
