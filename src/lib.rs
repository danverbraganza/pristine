#![forbid(unsafe_code)]

pub mod agent;
pub mod builtins;
pub mod config;
pub mod harness;
pub mod history;
pub mod messagebus;
pub mod model;
pub mod provider;
pub mod rpc;
pub mod shell;
pub mod stdio;
pub mod tool;
pub mod user;

pub mod test_support;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::agent::AgentId;
use crate::builtins::{Edit, ExecBash, Insert, Read, Write};
use crate::config::{
    Config, ConfigError, ConfigErrors, LoadArgs, ProviderConfig, load as load_config,
};
use crate::harness::{Harness, HarnessBuilder, ModelId, PendingAgent};
use crate::messagebus::MessageBus;
use crate::model::anthropic::AnthropicProvider;
use crate::model::deepseek::DeepSeekProvider;
use crate::model::openrouter::OpenRouterProvider;
use crate::provider::{ModelInstanceConfig, ModelProvider};

#[derive(Parser, Debug)]
#[command(name = "pristine", about = "Pristine agent harness")]
struct Cli {
    /// Path to a topology TOML file. When supplied, replaces the embedded
    /// `default.toml` entirely (no merging). Accepted at any position.
    #[arg(short = 'c', long = "config", global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Path to the auth TOML file. When supplied, overrides the default
    /// `<home>/pristine-auth.toml` location. Accepted at any position.
    #[arg(long = "auth", global = true, value_name = "PATH")]
    auth: Option<PathBuf>,

    /// Override the model alias every topology agent resolves against. Absent
    /// = use the alias declared in the topology.
    #[arg(long = "model", global = true, value_name = "ALIAS")]
    model: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the JSON-RPC stdio server.
    Run,
}

/// Entry point shared by the `pristine` and `1p` binaries.
///
/// Constructs a multi-threaded Tokio runtime with two worker threads (the floor
/// required to avoid deadlocks between the streaming HTTP body task and the
/// Agent event loop) and drives the async entry point to completion.
pub fn run() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(async { run_async().await })
}

async fn run_async() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let command = cli
        .command
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("a subcommand is required; try `pristine run`"))?;
    let Command::Run = command;

    let config = match load_config(LoadArgs {
        config: cli.config.as_deref(),
        auth: cli.auth.as_deref(),
        model: cli.model.as_deref(),
    }) {
        Ok(c) => c,
        Err(errors) => {
            eprintln!("{errors}");
            std::process::exit(1);
        }
    };

    let (mut harness, agent_ids) = match build_harness_from_config(config) {
        Ok(value) => value,
        Err(HarnessAssemblyError::Config(errors)) => {
            eprintln!("{errors}");
            std::process::exit(1);
        }
        Err(HarnessAssemblyError::Other(err)) => return Err(err),
    };
    let primary_agent_id = agent_ids
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("config produced no agents; nothing to run"))?;

    harness.start()?;

    let shutdown_token = tokio_util::sync::CancellationToken::new();
    let owner_id = harness.owner_id();
    let bus = harness.bus().clone() as Arc<dyn MessageBus>;

    crate::stdio::run_server(bus, primary_agent_id, owner_id, shutdown_token).await?;

    harness.shutdown();
    harness.join().await?;
    Ok(())
}

/// Failure modes for [`build_harness_from_config`]. Config-level failures
/// (an agent referencing a provider absent from the registry) are surfaced
/// as `ConfigErrors` so the caller can render them with the same machinery
/// it uses for the `load` path; everything else is collapsed into a single
/// opaque `anyhow::Error`.
#[derive(Debug)]
pub enum HarnessAssemblyError {
    Config(ConfigErrors),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for HarnessAssemblyError {
    fn from(err: anyhow::Error) -> Self {
        HarnessAssemblyError::Other(err)
    }
}

/// Walk a fully-resolved [`Config`] into a built [`Harness`].
///
/// Registers the `anthropic` provider, the five built-in tools, and one
/// `(model, agent)` pair per resolved agent. The returned `Vec<AgentId>`
/// preserves the order of `config.agents` so callers can identify the
/// primary agent (the stdio JSON-RPC server is single-agent today).
///
/// Provider-name resolution is the one validation step layered in here
/// rather than inside `pristine_config::load`: alias resolution leaves
/// `provider_name` as a free-form string, and only the binary's
/// `ProviderRegistry` knows which provider names are valid. Lookup misses
/// surface as `ConfigError::UnknownProvider` collected into a
/// `ConfigErrors` aggregate so multiple bad provider names render together.
pub fn build_harness_from_config(
    config: Config,
) -> Result<(Harness, Vec<AgentId>), HarnessAssemblyError> {
    // Maintain a parallel `HashMap<String, Arc<dyn ModelProvider>>` because
    // `HarnessBuilder` does not expose its `ProviderRegistry` pre-build. The
    // map is kept in lock-step with `HarnessBuilder::add_provider` calls so
    // unknown-provider validation can run against the same names the
    // harness will resolve at agent-build time.
    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    let anthropic: Arc<dyn ModelProvider> = Arc::new(AnthropicProvider::new());
    providers.insert("anthropic".to_string(), anthropic.clone());
    let deepseek: Arc<dyn ModelProvider> = Arc::new(DeepSeekProvider::new());
    providers.insert("deepseek".to_string(), deepseek.clone());
    let openrouter: Arc<dyn ModelProvider> = Arc::new(OpenRouterProvider::new());
    providers.insert("openrouter".to_string(), openrouter.clone());

    let mut builder = HarnessBuilder::new()
        .add_provider("anthropic", anthropic)
        .map_err(|e| anyhow::anyhow!("failed to register AnthropicProvider: {e}"))?
        .add_provider("deepseek", deepseek)
        .map_err(|e| anyhow::anyhow!("failed to register DeepSeekProvider: {e}"))?
        .add_provider("openrouter", openrouter)
        .map_err(|e| anyhow::anyhow!("failed to register OpenRouterProvider: {e}"))?;

    builder = register_builtin_tools(builder)?;

    let mut provider_errors = ConfigErrors::new();
    for agent in &config.agents {
        if !providers.contains_key(&agent.model.provider_name) {
            provider_errors.push(ConfigError::UnknownProvider {
                name: agent.model.provider_name.clone(),
            });
        }
    }
    if !provider_errors.is_empty() {
        return Err(HarnessAssemblyError::Config(provider_errors));
    }

    let mut agent_ids = Vec::with_capacity(config.agents.len());
    for (idx, agent) in config.agents.iter().enumerate() {
        let provider = providers.get(&agent.model.provider_name).ok_or_else(|| {
            anyhow::anyhow!(
                "internal: provider '{}' disappeared from registry after validation",
                agent.model.provider_name
            )
        })?;

        let mut extras = serde_json::Map::new();
        extras.insert(
            "api_key".to_string(),
            serde_json::Value::String(agent.model.api_key.clone()),
        );
        match config.providers.get(&agent.model.provider_name) {
            Some(ProviderConfig::Anthropic {
                base_url: Some(url),
            })
            | Some(ProviderConfig::DeepSeek {
                base_url: Some(url),
            })
            | Some(ProviderConfig::OpenRouter {
                base_url: Some(url),
            }) => {
                extras.insert(
                    "base_url".to_string(),
                    serde_json::Value::String(url.clone()),
                );
            }
            _ => {}
        }
        let instance = ModelInstanceConfig::new(
            agent.model.model_name.clone(),
            serde_json::Value::Object(extras),
        );
        let model = provider.build_model(instance).map_err(|e| {
            anyhow::anyhow!("failed to build model for agent '{}': {e}", agent.name)
        })?;

        let model_id = ModelId::new(format!("{}-{}", agent.model.alias, idx));
        let agent_id = AgentId::new();
        builder = builder
            .add_model(model_id.clone(), model)
            .add_agent(PendingAgent {
                id: agent_id,
                system_prompt: agent.system_prompt.clone(),
                model_id,
            });
        agent_ids.push(agent_id);
    }

    let harness = builder
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build harness: {e}"))?;
    Ok((harness, agent_ids))
}

/// Register the five built-in tools shipped with pristine. Kept here, rather
/// than driven by `Config.tools`; a future bead may fold these into a
/// `Config.tools`-driven registration loop.
fn register_builtin_tools(builder: HarnessBuilder) -> anyhow::Result<HarnessBuilder> {
    let builder = builder
        .add_tool(Arc::new(ExecBash::new()))
        .map_err(|e| anyhow::anyhow!("failed to register ExecBash: {e}"))?
        .add_tool(Arc::new(Edit::new()))
        .map_err(|e| anyhow::anyhow!("failed to register Edit: {e}"))?
        .add_tool(Arc::new(Read::new()))
        .map_err(|e| anyhow::anyhow!("failed to register Read: {e}"))?
        .add_tool(Arc::new(Write::new()))
        .map_err(|e| anyhow::anyhow!("failed to register Write: {e}"))?
        .add_tool(Arc::new(Insert::new()))
        .map_err(|e| anyhow::anyhow!("failed to register Insert: {e}"))?;
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ResolvedAgent, ResolvedModel};
    use clap::Parser;
    use std::collections::HashMap;

    #[test]
    fn cli_accepts_config_flag() {
        let cli = Cli::try_parse_from(["pristine", "-c", "/tmp/topo.toml", "run"])
            .expect("CLI accepts -c flag before the subcommand");
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/tmp/topo.toml"))
        );
        assert!(cli.auth.is_none());
        assert!(matches!(cli.command, Some(Command::Run)));
    }

    #[test]
    fn cli_accepts_auth_flag() {
        let cli = Cli::try_parse_from(["pristine", "--auth", "/tmp/auth.toml", "run"])
            .expect("CLI accepts --auth flag before the subcommand");
        assert_eq!(
            cli.auth.as_deref(),
            Some(std::path::Path::new("/tmp/auth.toml"))
        );
        assert!(cli.config.is_none());
        assert!(matches!(cli.command, Some(Command::Run)));
    }

    #[test]
    fn cli_accepts_both_flags() {
        let cli = Cli::try_parse_from([
            "pristine",
            "-c",
            "/tmp/topo.toml",
            "--auth",
            "/tmp/auth.toml",
            "run",
        ])
        .expect("CLI accepts both global flags together");
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/tmp/topo.toml"))
        );
        assert_eq!(
            cli.auth.as_deref(),
            Some(std::path::Path::new("/tmp/auth.toml"))
        );
        assert!(matches!(cli.command, Some(Command::Run)));
    }

    #[test]
    fn cli_accepts_model_flag() {
        let cli = Cli::try_parse_from(["pristine", "--model", "openrouter.deepseek", "run"])
            .expect("CLI accepts --model flag before the subcommand");
        assert_eq!(cli.model.as_deref(), Some("openrouter.deepseek"));
        assert!(matches!(cli.command, Some(Command::Run)));
    }

    #[test]
    fn cli_global_flag_at_any_position() {
        let cli = Cli::try_parse_from(["pristine", "run", "-c", "/tmp/topo.toml"])
            .expect("CLI accepts -c flag in subcommand position via global = true");
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/tmp/topo.toml"))
        );
        assert!(matches!(cli.command, Some(Command::Run)));
    }

    fn anthropic_provider_only() -> HashMap<String, ProviderConfig> {
        let mut providers = HashMap::new();
        providers.insert(
            "anthropic".to_string(),
            ProviderConfig::Anthropic { base_url: None },
        );
        providers
    }

    fn one_agent_config() -> Config {
        Config {
            agents: vec![ResolvedAgent {
                name: "default".to_string(),
                system_prompt: "test prompt".to_string(),
                tools: vec!["read".to_string(), "write".to_string()],
                model: ResolvedModel {
                    alias: "default".to_string(),
                    provider_name: "anthropic".to_string(),
                    model_name: "claude-sonnet-4-6".to_string(),
                    api_key: "sk-test".to_string(),
                },
            }],
            tools: HashMap::new(),
            providers: anthropic_provider_only(),
        }
    }

    #[test]
    fn build_harness_from_config_happy_path_registers_one_agent_and_five_tools()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = one_agent_config();
        let result = build_harness_from_config(config);
        let (harness, agent_ids) = match result {
            Ok(value) => value,
            Err(HarnessAssemblyError::Config(errors)) => {
                return Err(format!("expected Ok build, got Config errors: {errors}").into());
            }
            Err(HarnessAssemblyError::Other(err)) => {
                return Err(format!("expected Ok build, got Other: {err}").into());
            }
        };

        assert_eq!(agent_ids.len(), 1);
        assert!(harness.provider_registry().get("anthropic").is_some());

        let mut tool_names: Vec<String> = harness
            .tools()
            .list()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        tool_names.sort();
        assert_eq!(
            tool_names,
            vec![
                "edit".to_string(),
                "exec_bash".to_string(),
                "insert".to_string(),
                "read".to_string(),
                "write".to_string(),
            ],
            "expected the five built-in tools registered",
        );
        Ok(())
    }

    #[test]
    fn build_harness_from_config_registers_deepseek_provider()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut providers = HashMap::new();
        providers.insert(
            "deepseek".to_string(),
            ProviderConfig::DeepSeek { base_url: None },
        );
        let config = Config {
            agents: vec![ResolvedAgent {
                name: "default".to_string(),
                system_prompt: "test prompt".to_string(),
                tools: vec!["read".to_string(), "write".to_string()],
                model: ResolvedModel {
                    alias: "default".to_string(),
                    provider_name: "deepseek".to_string(),
                    model_name: "deepseek-v4-flash".to_string(),
                    api_key: "sk-test".to_string(),
                },
            }],
            tools: HashMap::new(),
            providers,
        };
        let (harness, agent_ids) = match build_harness_from_config(config) {
            Ok(value) => value,
            Err(HarnessAssemblyError::Config(errors)) => {
                return Err(format!("expected Ok build, got Config errors: {errors}").into());
            }
            Err(HarnessAssemblyError::Other(err)) => {
                return Err(format!("expected Ok build, got Other: {err}").into());
            }
        };
        assert_eq!(agent_ids.len(), 1);
        assert!(harness.provider_registry().get("deepseek").is_some());
        Ok(())
    }

    #[test]
    fn build_harness_from_config_registers_openrouter_provider()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut providers = HashMap::new();
        providers.insert(
            "openrouter".to_string(),
            ProviderConfig::OpenRouter { base_url: None },
        );
        let config = Config {
            agents: vec![ResolvedAgent {
                name: "default".to_string(),
                system_prompt: "test prompt".to_string(),
                tools: vec!["read".to_string(), "write".to_string()],
                model: ResolvedModel {
                    alias: "default".to_string(),
                    provider_name: "openrouter".to_string(),
                    model_name: "anthropic/claude-3.5-sonnet".to_string(),
                    api_key: "sk-test".to_string(),
                },
            }],
            tools: HashMap::new(),
            providers,
        };
        let (harness, agent_ids) = match build_harness_from_config(config) {
            Ok(value) => value,
            Err(HarnessAssemblyError::Config(errors)) => {
                return Err(format!("expected Ok build, got Config errors: {errors}").into());
            }
            Err(HarnessAssemblyError::Other(err)) => {
                return Err(format!("expected Ok build, got Other: {err}").into());
            }
        };
        assert_eq!(agent_ids.len(), 1);
        assert!(harness.provider_registry().get("openrouter").is_some());
        Ok(())
    }

    #[test]
    fn build_harness_from_config_zero_agents_yields_empty_agent_id_list()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = Config {
            agents: Vec::new(),
            tools: HashMap::new(),
            providers: anthropic_provider_only(),
        };
        let (_harness, agent_ids) = match build_harness_from_config(config) {
            Ok(value) => value,
            Err(HarnessAssemblyError::Config(errors)) => {
                return Err(format!("expected Ok build, got Config errors: {errors}").into());
            }
            Err(HarnessAssemblyError::Other(err)) => {
                return Err(format!("expected Ok build, got Other: {err}").into());
            }
        };
        assert!(agent_ids.is_empty());
        Ok(())
    }

    #[test]
    fn build_harness_from_config_unknown_provider_yields_config_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut config = one_agent_config();
        config.agents[0].model.provider_name = "nonesuch".to_string();
        match build_harness_from_config(config) {
            Ok(_) => return Err("unknown provider must fail".into()),
            Err(HarnessAssemblyError::Config(errors)) => {
                let mut saw_unknown = false;
                for entry in errors.as_slice() {
                    if let ConfigError::UnknownProvider { name } = entry {
                        assert_eq!(name, "nonesuch");
                        saw_unknown = true;
                    }
                }
                assert!(
                    saw_unknown,
                    "expected ConfigError::UnknownProvider; got {errors}"
                );
            }
            Err(HarnessAssemblyError::Other(e)) => {
                return Err(format!("expected Config variant, got Other: {e}").into());
            }
        }
        Ok(())
    }

    #[test]
    fn build_harness_from_config_multiple_agents_get_distinct_model_ids()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut config = one_agent_config();
        let second = ResolvedAgent {
            name: "second".to_string(),
            system_prompt: "second prompt".to_string(),
            tools: Vec::new(),
            model: ResolvedModel {
                alias: "default".to_string(),
                provider_name: "anthropic".to_string(),
                model_name: "claude-sonnet-4-6".to_string(),
                api_key: "sk-test-2".to_string(),
            },
        };
        config.agents.push(second);

        let (_harness, agent_ids) = match build_harness_from_config(config) {
            Ok(value) => value,
            Err(HarnessAssemblyError::Config(errors)) => {
                return Err(format!("expected Ok build, got Config errors: {errors}").into());
            }
            Err(HarnessAssemblyError::Other(err)) => {
                return Err(format!("expected Ok build, got Other: {err}").into());
            }
        };
        assert_eq!(agent_ids.len(), 2);
        assert_ne!(agent_ids[0], agent_ids[1]);
        Ok(())
    }
}
