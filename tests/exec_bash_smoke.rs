//! End-to-end smoke tests for `ExecBash` that exercise a real `/bin/bash`.
//!
//! All tests in this file are `#[ignore]`-gated so the default
//! `cargo nextest run` does not depend on the host shell. Run explicitly with:
//!
//!     cargo nextest run --run-ignored=only -E 'test(exec_bash_smoke)'

use serde_json::json;

use pristine::builtins::ExecBash;
use pristine::tool::Tool;

#[tokio::test]
#[ignore = "spawns real /bin/bash; opt-in"]
async fn exec_bash_smoke_echoes_string() {
    let tool = ExecBash::new();
    let value = tool
        .call(json!({"command": "echo hello-world"}))
        .await
        .expect("real bash echo succeeds");

    assert_eq!(value["status"]["status"], "exit");
    assert_eq!(value["status"]["code"], 0);
    let stdout = value["stdout"].as_str().expect("stdout is a string");
    assert!(
        stdout.contains("hello-world"),
        "expected 'hello-world' in stdout, got {stdout:?}",
    );
}

#[tokio::test]
#[ignore = "spawns real /bin/bash; opt-in"]
async fn exec_bash_smoke_non_zero_exit() {
    let tool = ExecBash::new();
    let value = tool
        .call(json!({"command": "exit 7"}))
        .await
        .expect("non-zero exit is Ok at the tool layer");

    assert_eq!(value["status"]["status"], "exit");
    assert_eq!(value["status"]["code"], 7);
}

#[tokio::test]
#[ignore = "spawns real /bin/bash; opt-in"]
async fn exec_bash_smoke_timeout_kills_process() {
    let tool = ExecBash::new();
    let value = tool
        .call(json!({"command": "sleep 5", "timeout_seconds": 1}))
        .await
        .expect("timeout is Ok at the tool layer");

    assert_eq!(value["status"]["status"], "timeout");
}
