#![forbid(unsafe_code)]

pub mod agent;
pub mod harness;
pub mod history;
pub mod messagebus;
pub mod model;
pub mod user;

use std::io::Write;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tokio::sync::broadcast::error::RecvError;

use crate::agent::AgentId;
use crate::harness::{HarnessBuilder, ModelId, PendingAgent};
use crate::messagebus::AgentEvent;
use crate::model::anthropic::AnthropicModelBuilder;

const SYSTEM_PROMPT: &str =
    "You are the Pristine agent. You have an identity that is uniquely yours!";
const SECOND_MESSAGE: &str = "Write me a poem of what it is like to be you, Pristine";
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
    /// Send a message to the Pristine agent and stream the reply.
    Run {
        /// First user message sent from the Owner to the agent.
        message: String,
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
    let command = cli.command.ok_or_else(|| {
        anyhow::anyhow!("a subcommand is required; try `pristine run \"<message>\"`")
    })?;
    let Command::Run { message, model } = command;

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
        .build()?;

    harness.start()?;

    let mut rx = harness.subscribe(agent_id)?;
    let owner_id = harness.owner_id();
    harness.send_to_agent(agent_id, owner_id, message)?;

    let mut runs_completed = 0u32;
    let mut second_sent = false;

    while runs_completed < 2 {
        match rx.recv().await {
            Ok(AgentEvent::TokenDelta { text }) => {
                print!("{text}");
                std::io::stdout().flush()?;
            }
            Ok(AgentEvent::RunComplete { usage }) => {
                println!();
                eprintln!(
                    "[usage: input_tokens={} output_tokens={}]",
                    usage.input_tokens, usage.output_tokens
                );
                runs_completed += 1;
                if !second_sent {
                    harness.send_to_agent(agent_id, owner_id, SECOND_MESSAGE.to_string())?;
                    second_sent = true;
                }
            }
            Ok(AgentEvent::BlockComplete { .. }) => {}
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        }
    }

    harness.shutdown();
    harness.run_until_shutdown().await?;
    Ok(())
}
