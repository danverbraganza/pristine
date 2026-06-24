//! End-to-end chained live integration test for the built-in tools.
//!
//! Builds a real `Harness` with all five filesystem/shell tools
//! registered, wires it to a real Anthropic `ARModel`, and drives one
//! inbound `UserMessage` instructing the agent to Read a Python
//! fixture, Edit it, then ExecBash to run it. Asserts
//! that the file was modified, that an ExecBash tool call ran, and that
//! the agent's final `AgentMessage` mentions the expected output digit.
//!
//! `#[ignore]`-gated and `ANTHROPIC_API_KEY`-guarded. Run explicitly:
//!
//!     cargo nextest run --run-ignored=only -E 'test(builtin_tools_live)'

use std::env;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::time::timeout;
use uuid::Uuid;

use pristine::agent::{AgentId, SystemPrompt};
use pristine::builtins::{Edit, ExecBash, Insert, Read, Write};
use pristine::harness::{HarnessBuilder, ModelId, PendingAgent};
use pristine::history::Block;
use pristine::messagebus::AgentEvent;
use pristine::model::anthropic::AnthropicProvider;
use pristine::provider::{ModelInstanceConfig, ModelProvider};

const SYSTEM_PROMPT: &str =
    "You are the Pristine agent. You have an identity that is uniquely yours!";
const MODEL_NAME: &str = "claude-sonnet-4-6";
const ANTHROPIC_MODEL_KEY: &str = "anthropic-default";
const DRAIN_TIMEOUT: Duration = Duration::from_secs(60);

#[tokio::test]
#[ignore = "live API, requires ANTHROPIC_API_KEY"]
async fn builtin_tools_live_read_edit_exec() -> Result<(), Box<dyn std::error::Error>> {
    // Guard: skip cleanly if the live credential is not present. The
    // `#[ignore]` attribute already excludes this test from the default
    // run; this belt-and-suspenders check protects ad-hoc invocations
    // such as `--run-ignored=only` in an unconfigured shell.
    let api_key = match env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("ANTHROPIC_API_KEY not set; skipping live test");
            return Ok(());
        }
    };

    // Fixture: unique tempdir under the system temp root, holding a
    // single Python source file whose only statement prints `1+1`.
    let workdir = env::temp_dir().join(format!(
        "pristine-builtin-tools-live-{}",
        Uuid::new_v4().simple()
    ));
    fs::create_dir_all(&workdir).expect("create tempdir");
    let fixture_path = workdir.join("fixture.py");
    fs::write(&fixture_path, "print(1+1)\n").expect("write fixture.py");
    let fixture_path_str = fixture_path
        .to_str()
        .expect("fixture path is utf-8")
        .to_string();

    // Harness construction mirrors `src/lib.rs::run_async` -- the
    // canonical five-tool registration plus a real Anthropic ARModel.
    let anthropic = AnthropicProvider::new()
        .build_model(ModelInstanceConfig::new(
            MODEL_NAME,
            serde_json::json!({ "api_key": api_key }),
        ))
        .expect("anthropic model builds");

    let model_id = ModelId::new(ANTHROPIC_MODEL_KEY);
    let agent_id = AgentId::new();
    let mut harness = HarnessBuilder::new()
        .add_model(model_id.clone(), anthropic)
        .add_agent(PendingAgent {
            id: agent_id,
            system_prompt: SystemPrompt {
                base: SYSTEM_PROMPT.to_string(),
                skills: None,
            },
            model_id,
        })
        .add_tool(Arc::new(ExecBash::new()))
        .expect("register ExecBash")
        .add_tool(Arc::new(Edit::new()))
        .expect("register Edit")
        .add_tool(Arc::new(Read::new()))
        .expect("register Read")
        .add_tool(Arc::new(Write::new()))
        .expect("register Write")
        .add_tool(Arc::new(Insert::new()))
        .expect("register Insert")
        .build()
        .expect("harness builds");

    harness.start().expect("harness starts");

    let mut subscription = harness.subscribe(agent_id).expect("subscribe");
    let owner_id = harness.owner_id();

    let user_message = format!(
        "Read the file at {path} (use the Read tool with that absolute path), then use the \
         Edit tool to replace the exact string `1+1` with `2+2` in that same file, then use the \
         ExecBash tool to run `python3 {path}`. Finally, tell me the number that was printed.",
        path = fixture_path_str,
    );

    harness
        .send_to_agent(agent_id, owner_id, user_message)
        .expect("send_to_agent");

    // Drain events until Idle (or the 60s overall cap fires). Collect
    // both the BlockComplete payloads (for tool-call assertion) and the
    // final agent-message text (for the "4" substring assertion).
    let mut saw_exec_bash_call = false;
    let mut final_agent_text: Option<String> = None;
    let mut events_seen: Vec<&'static str> = Vec::new();

    let drain = async {
        while let Some(event) = subscription.next().await {
            match &event {
                AgentEvent::TokenDelta { .. } => events_seen.push("TokenDelta"),
                AgentEvent::ReasoningDelta { .. } => events_seen.push("ReasoningDelta"),
                AgentEvent::BlockComplete { block } => {
                    events_seen.push("BlockComplete");
                    match block.block() {
                        Block::ToolCall { name, .. } if name == "exec_bash" => {
                            saw_exec_bash_call = true;
                        }
                        Block::AgentMessage { content, .. } => {
                            final_agent_text = Some(content.clone());
                        }
                        _ => {}
                    }
                }
                AgentEvent::RunComplete { .. } => events_seen.push("RunComplete"),
                AgentEvent::Error { message } => {
                    return Err(format!("agent emitted Error event: {message}").into());
                }
                AgentEvent::Idle => {
                    events_seen.push("Idle");
                    break;
                }
            }
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    };

    match timeout(DRAIN_TIMEOUT, drain).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(format!(
                "drain timed out after {DRAIN_TIMEOUT:?}; events observed before timeout: {events_seen:?}"
            )
            .into());
        }
    }

    harness.shutdown();
    let _ = timeout(Duration::from_secs(5), harness.join()).await;

    // File-content assertion: strict equality. The Edit tool's contract
    // is exact-byte replacement, so the only acceptable result is the
    // original content with `1+1` swapped for `2+2`.
    let on_disk = fs::read_to_string(&fixture_path).expect("read fixture.py after run");
    assert_eq!(
        on_disk, "print(2+2)\n",
        "file content after agent run was not the expected edit",
    );

    // Tool-call assertion: at least one ToolCall block with name
    // `exec_bash` flowed through the event stream.
    assert!(
        saw_exec_bash_call,
        "expected at least one BlockComplete for an exec_bash ToolCall in the event stream",
    );

    // Final-message assertion: the digit `4` is present somewhere in
    // the agent's final text. Substring check, not equality, because
    // wording varies across runs.
    let final_text =
        final_agent_text.expect("expected at least one AgentMessage BlockComplete in the stream");
    assert!(
        final_text.contains('4'),
        "expected final agent message to contain '4', got: {final_text:?}",
    );

    Ok(())
}
