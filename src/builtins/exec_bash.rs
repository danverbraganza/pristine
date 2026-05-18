use crate::shell::ExecStatus;
use crate::tool::ToolError;

#[allow(dead_code)]
#[derive(serde::Deserialize)]
struct ExecBashInput {
    command: String,
    timeout_seconds: Option<u64>,
}

#[allow(dead_code)]
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExecBashError {
    Spawn { reason: String },
    Io { reason: String },
    TmpFile { reason: String },
}

#[allow(dead_code)]
fn err(e: ExecBashError) -> ToolError {
    let value =
        serde_json::to_value(e).unwrap_or_else(|_| serde_json::json!({"kind": "internal_error"}));
    ToolError::Execution(value)
}

#[allow(dead_code)]
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

pub struct ExecBash;

impl ExecBash {
    pub fn new() -> Self {
        ExecBash
    }
}

impl Default for ExecBash {
    fn default() -> Self {
        Self::new()
    }
}
