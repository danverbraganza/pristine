#![forbid(unsafe_code)]

pub mod agent;
pub mod builtins;
pub mod harness;
pub mod history;
pub mod messagebus;
pub mod model;
pub mod rpc;
pub mod shell;
pub mod stdio;
pub mod tool;
pub mod user;

#[cfg(test)]
mod test_support;

use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::agent::AgentId;
use crate::builtins::{AddTool, Edit, ExecBash, Read, Write};
use crate::harness::{HarnessBuilder, ModelId, PendingAgent};
use crate::messagebus::MessageBus;
use crate::model::anthropic::AnthropicModelBuilder;

const SYSTEM_PROMPT: &str = "You are the Pristine agent. You have an identity that is uniquely yours! \
     You have access to an `add` tool that returns the sum of two numbers; use it when arithmetic is requested.";
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const ANTHROPIC_MODEL_KEY: &str = "anthropic-default";

#[derive(Parser, Debug)]
#[command(name = "pristine", about = "Pristine agent harness")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the JSON-RPC stdio server.
    Run {
        /// Anthropic model identifier.
        #[arg(long, default_value = DEFAULT_MODEL)]
        model: String,
    },
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
        .ok_or_else(|| anyhow::anyhow!("a subcommand is required; try `pristine run`"))?;
    let Command::Run { model } = command;

    let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    if api_key.is_empty() {
        return Err(anyhow::anyhow!("ANTHROPIC_API_KEY not set"));
    }

    let anthropic = AnthropicModelBuilder::new()
        .api_key(api_key)
        .model_name(model)
        .build()?;

    let model_id = ModelId::new(ANTHROPIC_MODEL_KEY);
    let agent_id = AgentId::new();
    let mut harness = HarnessBuilder::new()
        .add_model(model_id.clone(), Arc::new(anthropic))
        .add_agent(PendingAgent {
            id: agent_id,
            system_prompt: SYSTEM_PROMPT.to_string(),
            model_id,
        })
        .add_tool(Arc::new(AddTool::new()))
        .map_err(|e| anyhow::anyhow!("failed to register AddTool: {e}"))?
        .add_tool(Arc::new(ExecBash::new()))
        .map_err(|e| anyhow::anyhow!("failed to register ExecBash: {e}"))?
        .add_tool(Arc::new(Edit::new()))
        .map_err(|e| anyhow::anyhow!("failed to register Edit: {e}"))?
        .add_tool(Arc::new(Read::new()))
        .map_err(|e| anyhow::anyhow!("failed to register Read: {e}"))?
        .add_tool(Arc::new(Write::new()))
        .map_err(|e| anyhow::anyhow!("failed to register Write: {e}"))?
        .build()?;

    harness.start()?;

    let shutdown_token = tokio_util::sync::CancellationToken::new();
    let owner_id = harness.owner_id();
    let bus = harness.bus().clone() as Arc<dyn MessageBus>;

    crate::stdio::run_server(bus, agent_id, owner_id, shutdown_token).await?;

    harness.shutdown();
    harness.join().await?;
    Ok(())
}
