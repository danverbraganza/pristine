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
pub use topology::{AgentConfig, ResolvedSkillsConfig, SkillsConfig, ToolConfig, TopologyConfig};
pub use validate::validate_tool_refs;

/// Canonical default topology shipped with pristine: the coding-assistant
/// prompt plus the built-in tools, embedded at compile time and used as
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
    /// Resolved skills configuration. `None` is the sole representation of
    /// "disabled" (absent `[skills]` block or explicit `enabled = false`);
    /// `Some` means enabled. Downstream gates on `skills.is_some()`.
    pub skills: Option<ResolvedSkillsConfig>,
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
///
/// `model_override`, when `Some`, rewrites the `model` alias of every agent in
/// the parsed topology before resolution runs, so all agents resolve against
/// the given alias instead of their declared one. The override value is opaque:
/// an alias absent from `auth.models` surfaces as `ConfigError::DanglingAlias`
/// exactly as a bad declared alias would.
pub fn assemble_config<E: EnvSource>(
    topology_input: &str,
    topology_path: &Path,
    auth_input: &str,
    auth_path: &Path,
    env: &E,
    model_override: Option<&str>,
) -> Result<Config, ConfigErrors> {
    let mut errors = ConfigErrors::new();

    let topology = match parse_topology_with_env(topology_input, topology_path, env) {
        Ok(mut value) => {
            if let Some(model) = model_override {
                for agent in &mut value.agents {
                    agent.model = model.to_string();
                }
            }
            Some(value)
        }
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

    let (topology, auth) = match (topology, auth) {
        (Some(t), Some(a)) => (t, a),
        _ => return Err(errors),
    };

    // Absent block ⇒ None; present block ⇒ resolve the kill-switch.
    let skills = topology.skills.and_then(|c| c.resolve());

    Ok(Config {
        agents: resolved_agents,
        tools: topology.tools.clone(),
        providers: auth.providers.clone(),
        skills,
    })
}

/// CLI-supplied inputs that select the topology and auth files for one call to
/// [`load`], plus an optional model-alias override. The path fields are
/// optional overrides; their `None` shapes select the embedded `default.toml`
/// (for `config`) and `<home>/pristine-auth.toml` (for `auth`). `model`, when
/// `Some`, overrides the model alias every topology agent resolves against;
/// `None` keeps each agent's declared alias.
///
/// Borrow-based to avoid `PathBuf`/`String` clones at the CLI boundary; the
/// binary owns the parsed clap values and passes them by reference.
#[derive(Debug, Clone, Copy)]
pub struct LoadArgs<'a> {
    /// CLI `-c/--config` override. `None` selects the embedded `default.toml`.
    pub config: Option<&'a Path>,
    /// CLI `--auth` override. `None` selects `<home>/pristine-auth.toml`.
    pub auth: Option<&'a Path>,
    /// CLI `--model` override. `None` keeps each topology agent's declared
    /// alias; `Some` rewrites every agent's alias before resolution.
    pub model: Option<&'a str>,
}

impl<'a> LoadArgs<'a> {
    /// Construct a `LoadArgs` with all overrides absent.
    pub fn new() -> Self {
        Self {
            config: None,
            auth: None,
            model: None,
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
        args.model,
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
mod tests;
