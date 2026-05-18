use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::shell::{BashShell, ExecStatus, Shell, ShellError};
use crate::tool::{Tool, ToolError};

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

fn err(e: ExecBashError) -> ToolError {
    let value =
        serde_json::to_value(e).unwrap_or_else(|_| serde_json::json!({"kind": "internal_error"}));
    ToolError::Execution(value)
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
        err(ExecBashError::TmpFile {
            reason: format!("create_dir_all: {e}"),
        })
    })?;
    // `set` returns Err if another thread won the race; either way the cell is
    // populated afterwards, so we ignore the result and read back.
    let _ = TMP_DIR.set(dir);
    TMP_DIR.get().ok_or_else(|| {
        err(ExecBashError::TmpFile {
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
    #[allow(dead_code)]
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
                    ShellError::Spawn(reason) => err(ExecBashError::Spawn { reason }),
                    ShellError::Io(reason) => err(ExecBashError::Io { reason }),
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
