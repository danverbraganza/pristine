use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::shell::{BashShell, ExecStatus, Shell, ShellError};
use crate::tool::{Tool, ToolError, execution_err};

#[derive(serde::Deserialize)]
struct ExecBashInput {
    command: String,
    timeout_seconds: Option<u64>,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExecBashError {
    Spawn { reason: String },
    Io { reason: String },
    TmpFile { reason: String },
}

#[derive(serde::Serialize)]
struct ExecBashOutput {
    stdout: String,
    stderr: String,
    status: ExecStatus,
    stdout_truncated: bool,
    stderr_truncated: bool,
    has_invalid_utf8_stdout: bool,
    has_invalid_utf8_stderr: bool,
    execution_id: String,
}

/// Maximum bytes of stdout/stderr returned in the JSON tool result. Output
/// longer than this is tail-truncated; the full bytes are staged on disk via
/// the per-process tmp directory.
const MAX_TAIL_BYTES: usize = 64 * 1024;

/// Default timeout applied when the caller does not specify `timeout_seconds`.
const DEFAULT_TIMEOUT_SECONDS: u64 = 30;

/// Per-process tmp directory for staged stdout/stderr. Initialized on first
/// use; lives for the lifetime of the process.
static TMP_DIR: OnceLock<PathBuf> = OnceLock::new();

fn ensure_tmp_dir() -> Result<&'static PathBuf, ToolError> {
    if let Some(dir) = TMP_DIR.get() {
        return Ok(dir);
    }
    // TODO: cleanup at shutdown (see PLAN.md "ExecBash tmp files" decision).
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("pristine-{pid}"));
    std::fs::create_dir_all(&dir).map_err(|e| {
        execution_err(ExecBashError::TmpFile {
            reason: format!("create_dir_all: {e}"),
        })
    })?;
    // `set` returns Err if another thread won the race; either way the cell is
    // populated afterwards, so we ignore the result and read back.
    let _ = TMP_DIR.set(dir);
    TMP_DIR.get().ok_or_else(|| {
        execution_err(ExecBashError::TmpFile {
            reason: "OnceLock unexpectedly empty".to_string(),
        })
    })
}

fn make_execution_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Tail-truncate to `MAX_TAIL_BYTES` and lossy-convert to UTF-8. Returns the
/// text, a `truncated` flag, and a `has_invalid_utf8` flag (true iff the tail
/// contained bytes that are not valid UTF-8 and were replaced by `U+FFFD`).
fn process_stream(bytes: &[u8]) -> (String, bool, bool) {
    let (tail, truncated) = if bytes.len() > MAX_TAIL_BYTES {
        (&bytes[bytes.len() - MAX_TAIL_BYTES..], true)
    } else {
        (bytes, false)
    };
    let has_invalid_utf8 = std::str::from_utf8(tail).is_err();
    let text = String::from_utf8_lossy(tail).into_owned();
    (text, truncated, has_invalid_utf8)
}

pub struct ExecBash {
    shell: Arc<dyn Shell + Send + Sync>,
    schema: Value,
}

impl ExecBash {
    pub fn new() -> Self {
        Self {
            shell: Arc::new(BashShell::new()),
            schema: Self::build_schema(),
        }
    }

    /// Test-only constructor that accepts an arbitrary `Shell` impl, used by
    /// `StubShell`-based unit tests in the T-3 slice.
    #[cfg(test)]
    pub(crate) fn with_shell(shell: Arc<dyn Shell + Send + Sync>) -> Self {
        Self {
            shell,
            schema: Self::build_schema(),
        }
    }

    fn build_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_seconds": {"type": "integer", "minimum": 0}
            },
            "required": ["command"]
        })
    }
}

impl Default for ExecBash {
    fn default() -> Self {
        Self::new()
    }
}

#[jsonrpsee::core::async_trait]
impl Tool for ExecBash {
    fn name(&self) -> &str {
        "exec_bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command and return captured stdout, stderr, and exit status. \
         Output is tail-truncated to 64 KiB; the full output is staged on disk under \
         the execution_id. Default timeout is 30 seconds."
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    async fn call(&self, input: Value) -> Result<Value, ToolError> {
        let parsed: ExecBashInput = serde_json::from_value(input).map_err(|e| {
            ToolError::InvalidInput(format!("ExecBash requires a 'command' string field: {e}"))
        })?;

        let timeout =
            Duration::from_secs(parsed.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS));

        let execution_id = make_execution_id();
        let tmp_dir = ensure_tmp_dir()?;
        let stdout_path = tmp_dir.join(format!("{execution_id}.stdout"));
        let stderr_path = tmp_dir.join(format!("{execution_id}.stderr"));

        let shell_output =
            self.shell
                .exec(&parsed.command, timeout)
                .await
                .map_err(|e| match e {
                    ShellError::Spawn(reason) => execution_err(ExecBashError::Spawn { reason }),
                    ShellError::Io(reason) => execution_err(ExecBashError::Io { reason }),
                })?;

        // NOTE: tmp-file staging is best-effort. The tool's primary contract is
        // the JSON tail; if writing the full output to disk fails (e.g., disk
        // full, permission denied) we log and proceed rather than failing the
        // shell call whose output the caller already paid for. Proper logging
        // (replacing eprintln!) is a future cycle.
        if let Err(e) = tokio::fs::write(&stdout_path, &shell_output.stdout).await {
            eprintln!("ExecBash: stdout tmp write failed ({stdout_path:?}): {e}");
        }
        if let Err(e) = tokio::fs::write(&stderr_path, &shell_output.stderr).await {
            eprintln!("ExecBash: stderr tmp write failed ({stderr_path:?}): {e}");
        }

        let (stdout, stdout_truncated, has_invalid_utf8_stdout) =
            process_stream(&shell_output.stdout);
        let (stderr, stderr_truncated, has_invalid_utf8_stderr) =
            process_stream(&shell_output.stderr);

        let output = ExecBashOutput {
            stdout,
            stderr,
            status: shell_output.status,
            stdout_truncated,
            stderr_truncated,
            has_invalid_utf8_stdout,
            has_invalid_utf8_stderr,
            execution_id,
        };

        Ok(serde_json::to_value(output)
            .unwrap_or_else(|_| json!({"error": "internal_serialization_failure"})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::ShellOutput;
    use crate::test_support::StubShell;

    fn stub_with(output: ShellOutput) -> Arc<StubShell> {
        Arc::new(StubShell::new(vec![Ok(output)]))
    }

    fn stub_err(error: ShellError) -> Arc<StubShell> {
        Arc::new(StubShell::new(vec![Err(error)]))
    }

    #[tokio::test]
    async fn exec_bash_happy_path_echo_zero_exit() {
        let stub = stub_with(ShellOutput {
            stdout: b"hello\n".to_vec(),
            stderr: Vec::new(),
            status: ExecStatus::Exit { code: 0 },
        });
        let tool = ExecBash::with_shell(stub);

        let value = tool
            .call(json!({"command": "echo hello"}))
            .await
            .expect("happy path returns Ok");

        assert_eq!(value["stdout"], "hello\n");
        assert_eq!(value["stderr"], "");
        assert_eq!(value["status"]["status"], "exit");
        assert_eq!(value["status"]["code"], 0);
        assert_eq!(value["stdout_truncated"], false);
        assert_eq!(value["stderr_truncated"], false);
        assert_eq!(value["has_invalid_utf8_stdout"], false);
        assert_eq!(value["has_invalid_utf8_stderr"], false);
        let execution_id = value["execution_id"].as_str().expect("execution_id string");
        assert_eq!(
            execution_id.len(),
            32,
            "execution_id should be 32 hyphenless chars, got {execution_id:?}",
        );
    }

    #[tokio::test]
    async fn exec_bash_non_zero_exit_is_not_a_tool_error() {
        let stub = stub_with(ShellOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: ExecStatus::Exit { code: 7 },
        });
        let tool = ExecBash::with_shell(stub);

        let value = tool
            .call(json!({"command": "exit 7"}))
            .await
            .expect("non-zero exit is Ok at the tool layer");

        assert_eq!(value["status"]["status"], "exit");
        assert_eq!(value["status"]["code"], 7);
    }

    #[tokio::test]
    async fn exec_bash_timeout_is_not_a_tool_error() {
        let stub = stub_with(ShellOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: ExecStatus::Timeout,
        });
        let tool = ExecBash::with_shell(stub);

        let value = tool
            .call(json!({"command": "sleep 99", "timeout_seconds": 1}))
            .await
            .expect("timeout is Ok at the tool layer");

        assert_eq!(value["status"]["status"], "timeout");
    }

    #[tokio::test]
    async fn exec_bash_signal_kill_status() {
        let stub = stub_with(ShellOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: ExecStatus::Signal {
                name: "SIGKILL".to_string(),
            },
        });
        let tool = ExecBash::with_shell(stub);

        let value = tool
            .call(json!({"command": "kill -9 $$"}))
            .await
            .expect("signal status is Ok at the tool layer");

        assert_eq!(value["status"]["status"], "signal");
        assert_eq!(value["status"]["name"], "SIGKILL");
    }

    #[tokio::test]
    async fn exec_bash_truncates_stdout_tail_to_64_kib() {
        let huge = vec![b'A'; 70 * 1024];
        let stub = stub_with(ShellOutput {
            stdout: huge,
            stderr: Vec::new(),
            status: ExecStatus::Exit { code: 0 },
        });
        let tool = ExecBash::with_shell(stub);

        let value = tool
            .call(json!({"command": "yes A"}))
            .await
            .expect("call succeeds");

        let stdout = value["stdout"].as_str().expect("stdout is a string");
        assert_eq!(stdout.len(), 64 * 1024);
        assert!(stdout.starts_with('A'));
        assert_eq!(value["stdout_truncated"], true);
        assert_eq!(value["stderr_truncated"], false);
    }

    #[tokio::test]
    async fn exec_bash_lossy_utf8_marks_invalid_flag() {
        let stub = stub_with(ShellOutput {
            stdout: vec![0x48, 0x69, 0xFF, 0x80],
            stderr: Vec::new(),
            status: ExecStatus::Exit { code: 0 },
        });
        let tool = ExecBash::with_shell(stub);

        let value = tool
            .call(json!({"command": "printf '...'"}))
            .await
            .expect("call succeeds");

        let stdout = value["stdout"].as_str().expect("stdout is a string");
        assert!(
            stdout.contains('\u{FFFD}'),
            "expected U+FFFD in lossy stdout, got {stdout:?}",
        );
        assert_eq!(value["has_invalid_utf8_stdout"], true);
        assert_eq!(value["has_invalid_utf8_stderr"], false);
    }

    #[tokio::test]
    async fn exec_bash_spawn_error_maps_to_execution_dialect() {
        let stub = stub_err(ShellError::Spawn("no such command".to_string()));
        let tool = ExecBash::with_shell(stub);

        let err = tool
            .call(json!({"command": "nope"}))
            .await
            .expect_err("spawn error surfaces as ToolError::Execution");

        let value = match err {
            ToolError::Execution(v) => v,
            other => panic!("expected Execution, got {other:?}"),
        };
        assert_eq!(value["kind"], "spawn");
        let reason = value["reason"].as_str().expect("reason is a string");
        assert!(
            reason.contains("no such command"),
            "unexpected reason: {reason}",
        );
    }

    #[tokio::test]
    async fn exec_bash_io_error_maps_to_execution_dialect() {
        let stub = stub_err(ShellError::Io("read failure".to_string()));
        let tool = ExecBash::with_shell(stub);

        let err = tool
            .call(json!({"command": "anything"}))
            .await
            .expect_err("io error surfaces as ToolError::Execution");

        let value = match err {
            ToolError::Execution(v) => v,
            other => panic!("expected Execution, got {other:?}"),
        };
        assert_eq!(value["kind"], "io");
        let reason = value["reason"].as_str().expect("reason is a string");
        assert!(
            reason.contains("read failure"),
            "unexpected reason: {reason}",
        );
    }

    #[tokio::test]
    async fn exec_bash_missing_command_field_is_invalid_input() {
        // No `command` field. This is an engine-level deserialization failure;
        // ExecBash surfaces it as ToolError::InvalidInput, NOT as the per-tool
        // Execution dialect.
        let stub = Arc::new(StubShell::new(Vec::new()));
        let tool = ExecBash::with_shell(stub);

        let err = tool
            .call(json!({"timeout_seconds": 10}))
            .await
            .expect_err("missing command yields an error");

        match err {
            ToolError::InvalidInput(_) => {}
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exec_bash_default_timeout_is_30_seconds() {
        let stub = Arc::new(StubShell::new(vec![Ok(ShellOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: ExecStatus::Exit { code: 0 },
        })]));
        let tool = ExecBash::with_shell(stub.clone());

        let _ = tool
            .call(json!({"command": "true"}))
            .await
            .expect("call succeeds");

        assert_eq!(stub.last_timeout(), Some(Duration::from_secs(30)));
    }

    #[tokio::test]
    async fn exec_bash_respects_caller_supplied_timeout() {
        let stub = Arc::new(StubShell::new(vec![Ok(ShellOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: ExecStatus::Exit { code: 0 },
        })]));
        let tool = ExecBash::with_shell(stub.clone());

        let _ = tool
            .call(json!({"command": "true", "timeout_seconds": 5}))
            .await
            .expect("call succeeds");

        assert_eq!(stub.last_timeout(), Some(Duration::from_secs(5)));
    }
}
