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
pub mod skills;
pub mod stdio;
pub mod tool;
pub mod user;

pub mod test_support;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::agent::{AgentId, SystemPrompt};
use crate::builtins::{ActivateSkill, Edit, ExecBash, Exit, Fork, Insert, Read, Write};
use crate::config::{
    Config, ConfigError, ConfigErrors, LoadArgs, ProviderConfig, ToolConfig, load as load_config,
};
use crate::harness::{Harness, HarnessBuilder, ModelId, PendingAgent};
use crate::messagebus::MessageBus;
use crate::model::anthropic::AnthropicProvider;
use crate::model::deepseek::DeepSeekProvider;
use crate::model::openrouter::OpenRouterProvider;
use crate::provider::{ModelInstanceConfig, ModelProvider};
use crate::skills::{SkillsRegistry, SkillsRegistrySource};
use crate::tool::Tool;

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

    /// Grant trust to project-scope skills for this invocation. Absent =
    /// project-scope skill paths are skipped and each is recorded as a
    /// `bypassed_path` diagnostic. Accepted at any position.
    #[arg(long = "trust-project-skills", global = true)]
    trust_project_skills: bool,

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

    let (mut harness, agent_ids) = match build_harness_from_config(config, cli.trust_project_skills)
    {
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
    let skills_announcer = harness.skills_announcer();

    crate::stdio::run_server(
        bus,
        primary_agent_id,
        owner_id,
        shutdown_token,
        skills_announcer,
    )
    .await?;

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
/// The returned `Vec<AgentId>` preserves the order of `config.agents` so
/// callers can identify the primary agent (the stdio JSON-RPC server is
/// single-agent today).
///
/// Provider-name resolution is the one validation step layered in here
/// rather than inside `pristine_config::load`: alias resolution leaves
/// `provider_name` as a free-form string, and only the binary's
/// `ProviderRegistry` knows which provider names are valid. Lookup misses
/// surface as `ConfigError::UnknownProvider` collected into a
/// `ConfigErrors` aggregate so multiple bad provider names render together.
pub fn build_harness_from_config(
    config: Config,
    trust_project: bool,
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

    // Construct the skills registry once and share it across every agent's
    // system-prompt slot and the `activate_skill` builtin. `trust_project`
    // carries the `--trust-project-skills` flag through to discovery: when
    // false, project-scope paths are skipped and each is recorded as a
    // `bypassed_path` diagnostic. When `config.skills` is `None` (disabled) the
    // slot stays `None`, rendering is unchanged, and a declared `activate_skill`
    // fails to construct.
    let skills: Option<Arc<dyn SkillsRegistrySource>> = config.skills.as_ref().map(|resolved| {
        Arc::new(SkillsRegistry::new(resolved.clone(), trust_project))
            as Arc<dyn SkillsRegistrySource>
    });

    let builtin_ctx = BuiltinContext {
        skills_registry: skills.clone(),
    };
    builder = register_builtin_tools(builder, &config.tools, &builtin_ctx)?;

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
                system_prompt: SystemPrompt {
                    base: agent.system_prompt.clone(),
                    skills: skills.clone(),
                },
                model_id,
            });
        agent_ids.push(agent_id);
    }

    if let Some(source) = &skills {
        builder = builder.skills(source.clone());
    }

    let harness = builder
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build harness: {e}"))?;
    Ok((harness, agent_ids))
}

/// Dependencies a builtin constructor closure may need at registration time.
///
/// Most builtins take no dependencies and ignore `ctx`. `activate_skill`
/// reads `skills_registry`: it is `Some` iff `[skills]` is
/// enabled, and a closure that needs it but finds `None` returns an error so a
/// declared-but-skills-absent config surfaces as a harness assembly failure.
struct BuiltinContext {
    skills_registry: Option<Arc<dyn SkillsRegistrySource>>,
}

/// Constructor closure for a single builtin tool, keyed by its `Tool::name()`.
type BuiltinCtor = Box<dyn Fn(&BuiltinContext) -> anyhow::Result<Arc<dyn Tool>>>;

/// Name-keyed dispatch table of builtin tool constructors. Keys are the
/// `Tool::name()` values the corresponding `[tools.X]` config declarations
/// reference; each value constructs the builtin as an `Arc<dyn Tool>`.
fn builtin_constructors() -> HashMap<&'static str, BuiltinCtor> {
    let mut table: HashMap<&'static str, BuiltinCtor> = HashMap::new();
    table.insert(
        "read",
        Box::new(|_ctx: &BuiltinContext| Ok(Arc::new(Read::new()) as Arc<dyn Tool>)),
    );
    table.insert(
        "write",
        Box::new(|_ctx: &BuiltinContext| Ok(Arc::new(Write::new()) as Arc<dyn Tool>)),
    );
    table.insert(
        "edit",
        Box::new(|_ctx: &BuiltinContext| Ok(Arc::new(Edit::new()) as Arc<dyn Tool>)),
    );
    table.insert(
        "insert",
        Box::new(|_ctx: &BuiltinContext| Ok(Arc::new(Insert::new()) as Arc<dyn Tool>)),
    );
    table.insert(
        "exec_bash",
        Box::new(|_ctx: &BuiltinContext| Ok(Arc::new(ExecBash::new()) as Arc<dyn Tool>)),
    );
    table.insert(
        "fork",
        Box::new(|_ctx: &BuiltinContext| Ok(Arc::new(Fork::new()) as Arc<dyn Tool>)),
    );
    table.insert(
        "exit",
        Box::new(|_ctx: &BuiltinContext| Ok(Arc::new(Exit::new()) as Arc<dyn Tool>)),
    );
    table.insert(
        "activate_skill",
        Box::new(|ctx: &BuiltinContext| match &ctx.skills_registry {
            Some(reg) => Ok(Arc::new(ActivateSkill::new(reg.clone())) as Arc<dyn Tool>),
            None => Err(anyhow::anyhow!(
                "[tools.activate_skill] is declared but skills are not enabled; \
                 add a [skills] block (or remove [tools.activate_skill])"
            )),
        }),
    );
    table
}

/// Register the builtin tools declared in `tools`, driven by the dispatch table.
///
/// Iterates the dispatch table's known builtin names and registers a builtin
/// iff its name is a key present in `tools`. Iterating the table (not `tools`)
/// means unknown or extra `tools` entries are ignored.
fn register_builtin_tools(
    mut builder: HarnessBuilder,
    tools: &HashMap<String, ToolConfig>,
    ctx: &BuiltinContext,
) -> anyhow::Result<HarnessBuilder> {
    for (name, ctor) in builtin_constructors() {
        if !tools.contains_key(name) {
            continue;
        }
        let tool = ctor(ctx)?;
        builder = builder
            .add_tool(tool)
            .map_err(|e| anyhow::anyhow!("failed to register {name}: {e}"))?;
    }
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ResolvedAgent, ResolvedModel, ResolvedSkillsConfig, ToolConfig};
    use crate::test_support::SkillsFixture;
    use clap::Parser;
    use std::collections::HashMap;

    /// Build a `config.tools` map declaring the named builtins, mirroring the
    /// `[tools.X]` entries a topology file would carry.
    fn tool_configs(names: &[&str]) -> HashMap<String, ToolConfig> {
        names
            .iter()
            .map(|name| {
                (
                    name.to_string(),
                    ToolConfig::Builtin {
                        builtin: name.to_string(),
                    },
                )
            })
            .collect()
    }

    /// The builtin names declared by the shipped `default.toml`.
    fn all_builtin_tool_configs() -> HashMap<String, ToolConfig> {
        tool_configs(&["read", "write", "edit", "insert", "exec_bash"])
    }

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

    #[test]
    fn cli_accepts_trust_project_skills_flag() {
        let cli = Cli::try_parse_from(["pristine", "--trust-project-skills", "run"])
            .expect("CLI accepts --trust-project-skills before the subcommand");
        assert!(cli.trust_project_skills);
        assert!(matches!(cli.command, Some(Command::Run)));
    }

    #[test]
    fn cli_trust_project_skills_flag_after_subcommand() {
        let cli = Cli::try_parse_from(["pristine", "run", "--trust-project-skills"])
            .expect("CLI accepts --trust-project-skills in subcommand position via global = true");
        assert!(cli.trust_project_skills);
        assert!(matches!(cli.command, Some(Command::Run)));
    }

    #[test]
    fn cli_trust_project_skills_defaults_false_when_absent() {
        let cli =
            Cli::try_parse_from(["pristine", "run"]).expect("CLI parses with no optional flags");
        assert!(
            !cli.trust_project_skills,
            "the flag defaults to false when not passed",
        );
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
            tools: all_builtin_tool_configs(),
            providers: anthropic_provider_only(),
            skills: None,
        }
    }

    #[test]
    fn build_harness_from_config_happy_path_registers_one_agent_and_five_tools()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = one_agent_config();
        let result = build_harness_from_config(config, false);
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
    fn register_builtin_tools_subset_registers_only_declared()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut config = one_agent_config();
        config.tools = tool_configs(&["read", "write"]);
        let (harness, _agent_ids) = match build_harness_from_config(config, false) {
            Ok(value) => value,
            Err(HarnessAssemblyError::Config(errors)) => {
                return Err(format!("expected Ok build, got Config errors: {errors}").into());
            }
            Err(HarnessAssemblyError::Other(err)) => {
                return Err(format!("expected Ok build, got Other: {err}").into());
            }
        };
        let mut tool_names: Vec<String> = harness
            .tools()
            .list()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        tool_names.sort();
        assert_eq!(
            tool_names,
            vec!["read".to_string(), "write".to_string()],
            "only the declared subset registers",
        );
        Ok(())
    }

    #[test]
    fn register_builtin_tools_none_declared_registers_nothing()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut config = one_agent_config();
        config.tools = HashMap::new();
        let (harness, _agent_ids) = match build_harness_from_config(config, false) {
            Ok(value) => value,
            Err(HarnessAssemblyError::Config(errors)) => {
                return Err(format!("expected Ok build, got Config errors: {errors}").into());
            }
            Err(HarnessAssemblyError::Other(err)) => {
                return Err(format!("expected Ok build, got Other: {err}").into());
            }
        };
        assert!(
            harness.tools().list().is_empty(),
            "no declared tools means no builtins register",
        );
        Ok(())
    }

    #[test]
    fn register_builtin_tools_ignores_unknown_tool_names() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut config = one_agent_config();
        config.tools = tool_configs(&["read", "nonesuch"]);
        let (harness, _agent_ids) = match build_harness_from_config(config, false) {
            Ok(value) => value,
            Err(HarnessAssemblyError::Config(errors)) => {
                return Err(format!("expected Ok build, got Config errors: {errors}").into());
            }
            Err(HarnessAssemblyError::Other(err)) => {
                return Err(format!("expected Ok build, got Other: {err}").into());
            }
        };
        let tool_names: Vec<String> = harness
            .tools()
            .list()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(
            tool_names,
            vec!["read".to_string()],
            "unknown config tool names are ignored",
        );
        Ok(())
    }

    /// A `ResolvedSkillsConfig` pointed at `fixture`'s tempdir for the user
    /// scope, with project scope emptied so the (bypassed) defaults never leak
    /// real filesystem state into the test.
    fn enabled_skills_config(fixture: &SkillsFixture) -> ResolvedSkillsConfig {
        ResolvedSkillsConfig {
            user_paths: Some(vec![fixture.path().to_string_lossy().into_owned()]),
            project_paths: Some(Vec::new()),
            disabled: Vec::new(),
        }
    }

    #[tokio::test]
    async fn build_harness_with_skills_and_activate_skill_activates_end_to_end()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = SkillsFixture::new()?.add_skill("greet", "Greet the user", "# Greet\nhi")?;
        let mut config = one_agent_config();
        config.tools = tool_configs(&["read", "activate_skill"]);
        config.skills = Some(enabled_skills_config(&fixture));

        let (harness, _agent_ids) = match build_harness_from_config(config, false) {
            Ok(value) => value,
            Err(HarnessAssemblyError::Config(errors)) => {
                return Err(format!("expected Ok build, got Config errors: {errors}").into());
            }
            Err(HarnessAssemblyError::Other(err)) => {
                return Err(format!("expected Ok build, got Other: {err}").into());
            }
        };

        let mut tool_names: Vec<String> = harness
            .tools()
            .list()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        tool_names.sort();
        assert_eq!(
            tool_names,
            vec!["activate_skill".to_string(), "read".to_string()],
            "activate_skill registers when [skills] and [tools.activate_skill] both present",
        );

        let value = harness
            .tools()
            .dispatch("activate_skill", serde_json::json!({ "name": "greet" }))
            .await?;
        let body = value["body"].as_str().ok_or("body is a string")?;
        assert!(
            body.contains("# Greet") && body.contains("hi"),
            "activated body carries the SKILL.md markdown body, got {body:?}",
        );
        Ok(())
    }

    /// A `ResolvedSkillsConfig` with `fixture` mounted as the sole *project*
    /// scope path and the user scope emptied. Project-scope discovery hinges on
    /// the trust flag, so this config isolates the flag's effect.
    fn project_scope_skills_config(fixture: &SkillsFixture) -> ResolvedSkillsConfig {
        ResolvedSkillsConfig {
            user_paths: Some(Vec::new()),
            project_paths: Some(vec![fixture.path().to_string_lossy().into_owned()]),
            disabled: Vec::new(),
        }
    }

    #[tokio::test]
    async fn build_harness_trust_project_true_discovers_project_skill()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture =
            SkillsFixture::new()?.add_skill("proj", "Project-scope skill", "# Proj\nbody")?;
        let mut config = one_agent_config();
        config.tools = tool_configs(&["read", "activate_skill"]);
        config.skills = Some(project_scope_skills_config(&fixture));

        let (harness, _agent_ids) = match build_harness_from_config(config, true) {
            Ok(value) => value,
            Err(HarnessAssemblyError::Config(errors)) => {
                return Err(format!("expected Ok build, got Config errors: {errors}").into());
            }
            Err(HarnessAssemblyError::Other(err)) => {
                return Err(format!("expected Ok build, got Other: {err}").into());
            }
        };

        let value = harness
            .tools()
            .dispatch("activate_skill", serde_json::json!({ "name": "proj" }))
            .await?;
        let body = value["body"].as_str().ok_or("body is a string")?;
        assert!(
            body.contains("# Proj"),
            "trust granted: project-scope skill is discovered and activatable, got {body:?}",
        );
        Ok(())
    }

    #[tokio::test]
    async fn build_harness_trust_project_false_skips_project_skill_with_bypass_diagnostic()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture =
            SkillsFixture::new()?.add_skill("proj", "Project-scope skill", "# Proj\nbody")?;
        let mut config = one_agent_config();
        config.tools = tool_configs(&["read", "activate_skill"]);
        config.skills = Some(project_scope_skills_config(&fixture));

        let (harness, _agent_ids) = match build_harness_from_config(config, false) {
            Ok(value) => value,
            Err(HarnessAssemblyError::Config(errors)) => {
                return Err(format!("expected Ok build, got Config errors: {errors}").into());
            }
            Err(HarnessAssemblyError::Other(err)) => {
                return Err(format!("expected Ok build, got Other: {err}").into());
            }
        };

        // Without trust the project skill is invisible: activation reports the
        // skill as unknown rather than returning its body.
        let result = harness
            .tools()
            .dispatch("activate_skill", serde_json::json!({ "name": "proj" }))
            .await;
        assert!(
            result.is_err(),
            "no trust: project-scope skill must not be discovered or activatable",
        );

        // The bypass is recorded as a diagnostic on the shared registry.
        let project_path = fixture.path().to_path_buf();
        let registry = SkillsRegistry::new(project_scope_skills_config(&fixture), false);
        let diagnostics = registry.diagnostics();
        assert!(
            diagnostics.iter().any(|d| matches!(
                d,
                crate::skills::SkillDiagnostic::BypassedPath { path } if path == &project_path
            )),
            "no trust: each bypassed project path yields a bypassed_path diagnostic, got {diagnostics:?}",
        );
        Ok(())
    }

    #[test]
    fn build_harness_activate_skill_declared_without_skills_is_assembly_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut config = one_agent_config();
        config.tools = tool_configs(&["read", "activate_skill"]);
        // skills stays disabled (default), so the activate_skill closure finds
        // `BuiltinContext::skills_registry == None` and returns an error.
        match build_harness_from_config(config, false) {
            Ok(_) => Err("expected assembly error for declared-but-no-skills".into()),
            Err(HarnessAssemblyError::Other(_)) => Ok(()),
            Err(HarnessAssemblyError::Config(errors)) => {
                Err(format!("expected Other assembly error, got Config errors: {errors}").into())
            }
        }
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
            tools: all_builtin_tool_configs(),
            providers,
            skills: None,
        };
        let (harness, agent_ids) = match build_harness_from_config(config, false) {
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
            tools: all_builtin_tool_configs(),
            providers,
            skills: None,
        };
        let (harness, agent_ids) = match build_harness_from_config(config, false) {
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
            skills: None,
        };
        let (_harness, agent_ids) = match build_harness_from_config(config, false) {
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
        match build_harness_from_config(config, false) {
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

        let (_harness, agent_ids) = match build_harness_from_config(config, false) {
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
