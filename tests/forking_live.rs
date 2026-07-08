//! End-to-end live proof that a forked subagent actually runs and executes
//! tools.
//!
//! Builds a REAL `Harness` from the checked-in `forking.toml` topology,
//! wired to a real Anthropic model, and instructs the first agent to `fork` a
//! subagent whose sole job is to `write` a sentinel file and then `exit`.
//!
//! We assert two independent facts:
//!   1. an `agent_forked` event was emitted (a peer was actually spawned), and
//!   2. the sentinel file exists with the expected contents — which only the
//!      forked subagent was told to create (the parent is explicitly told not
//!      to write it).
//!
//! `#[ignore]`-gated and `ANTHROPIC_API_KEY`-guarded. Run explicitly:
//!
//!     cargo nextest run --run-ignored=only -E 'test(forking_live)'

use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::time::{Instant, sleep, timeout};
use uuid::Uuid;

use pristine::build_harness_from_config;
use pristine::config::{LoadArgs, load};
use pristine::messagebus::MessageBus;

const SENTINEL: &str = "FORKED_RAN";
// Sonnet is reliable at multi-step tool use (fork, then the child's write+exit).
const MODEL_NAME: &str = "claude-sonnet-4-6";
const OVERALL_DEADLINE: Duration = Duration::from_secs(90);

#[tokio::test]
#[ignore = "live API, requires ANTHROPIC_API_KEY"]
async fn forking_live_forked_subagent_runs_and_writes_a_file()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(api_key) = pristine::test_support::anthropic_key_or_skip() else {
        return Ok(());
    };

    let workdir =
        env::temp_dir().join(format!("pristine-forking-live-{}", Uuid::new_v4().simple()));
    fs::create_dir_all(&workdir)?;
    let sentinel_path = workdir.join("forked_output.txt");

    // The key is inlined (not templated) so the test does not depend on the
    // loader's env source beyond the guard above.
    let auth_path = workdir.join("auth.toml");
    fs::write(
        &auth_path,
        format!(
            "[providers.anthropic]\n\
             type = \"anthropic\"\n\n\
             [models.default]\n\
             provider = \"anthropic\"\n\
             model_name = \"{MODEL_NAME}\"\n\
             api_key = \"{api_key}\"\n"
        ),
    )?;

    let topology_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("forking.toml");

    let config = load(LoadArgs {
        config: Some(topology_path.as_path()),
        auth: Some(auth_path.as_path()),
        model: None,
    })
    .map_err(|e| format!("config load failed: {e}"))?;

    let (mut harness, agent_ids) = build_harness_from_config(config, false)
        .map_err(|e| format!("harness build failed: {e:?}"))?;
    let main_agent = *agent_ids.first().ok_or("forking.toml produced no agents")?;

    // Subscribe to fork events BEFORE anything is sent so we cannot miss it.
    let mut forks = harness.bus().subscribe_forks();

    harness.start().map_err(|e| format!("start failed: {e}"))?;
    let owner = harness.owner_id();

    let instruction = format!(
        "You are being tested. You have a `fork` tool that spawns a subagent. \
         Do EXACTLY this and nothing else: call the `fork` tool ONE time with \
         instruction = \"Use the write tool to create a file at the absolute path {path} \
         whose entire contents are exactly the text {sentinel} with no trailing newline. \
         Then call the exit tool.\" and tools = [\"write\", \"exit\"]. \
         Do NOT create the file yourself and do NOT call the write tool yourself — the \
         forked subagent must create it. Once you have called `fork` once, you are done.",
        path = sentinel_path.display(),
        sentinel = SENTINEL,
    );
    harness
        .send_to_agent(main_agent, owner, instruction)
        .map_err(|e| format!("send_to_agent failed: {e}"))?;

    // The harness is not shut down until after the assertions below, so the
    // peer is never cancelled mid-flight.
    let deadline = Instant::now() + OVERALL_DEADLINE;
    let mut fork_seen = false;
    let mut file_written = false;
    while Instant::now() < deadline {
        if !fork_seen
            && let Ok(Some(_event)) = timeout(Duration::from_millis(200), forks.next()).await
        {
            fork_seen = true;
        }
        if let Ok(contents) = fs::read_to_string(&sentinel_path)
            && contents.contains(SENTINEL)
        {
            file_written = true;
            break;
        }
        sleep(Duration::from_millis(300)).await;
    }

    harness.shutdown();
    let _ = timeout(Duration::from_secs(10), harness.join()).await;

    // Clean up before asserting so a failure never leaks the workdir.
    let _ = fs::remove_dir_all(&workdir);

    assert!(
        fork_seen,
        "no agent_forked event was observed — the parent never spawned a peer"
    );
    assert!(
        file_written,
        "the forked subagent never created {sentinel_path:?} with contents {SENTINEL:?} within \
         {OVERALL_DEADLINE:?} — no evidence it executed its write tool"
    );
    Ok(())
}
