//! Shell abstraction for command execution.
//!
//! The `Shell` trait is the portable shape: a single async `exec` method that
//! takes a command string and a timeout, and returns captured stdout/stderr
//! bytes plus an `ExecStatus`. The `BashShell` adapter dialect spawns
//! `/bin/bash -c <command>` and maps OS exit codes / signals onto `ExecStatus`.
//! A `StubShell` (in `test_support`) drives unit tests without spawning real
//! processes.

use std::process::Stdio;
use std::time::Duration;

use jsonrpsee::core::async_trait;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

#[async_trait]
pub trait Shell: Send + Sync {
    async fn exec(&self, command: &str, timeout: Duration) -> Result<ShellOutput, ShellError>;
}

#[derive(Debug)]
pub struct ShellOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: ExecStatus,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExecStatus {
    Exit { code: i32 },
    Signal { name: String },
    Timeout,
}

#[derive(Debug)]
pub enum ShellError {
    Spawn(String),
    Io(String),
}

impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShellError::Spawn(reason) => write!(f, "shell spawn error: {reason}"),
            ShellError::Io(reason) => write!(f, "shell I/O error: {reason}"),
        }
    }
}

impl std::error::Error for ShellError {}

/// Real shell adapter that spawns `/bin/bash -c <command>` via tokio.
pub struct BashShell;

impl BashShell {
    pub fn new() -> Self {
        Self
    }
}

impl Default for BashShell {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Shell for BashShell {
    async fn exec(
        &self,
        command: &str,
        timeout_duration: Duration,
    ) -> Result<ShellOutput, ShellError> {
        let mut child = Command::new("/bin/bash")
            .arg("-c")
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ShellError::Spawn(format!("{e}")))?;

        // Take the piped readers up-front and drain them concurrently into in-memory
        // buffers. Using wait_with_output would consume the child handle and prevent
        // a kill-on-timeout, so reader tasks are spawned and joined manually.
        let stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| ShellError::Io("stdout pipe missing".to_string()))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| ShellError::Io("stderr pipe missing".to_string()))?;

        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut reader = stdout_pipe;
            reader.read_to_end(&mut buf).await.map(|_| buf)
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut reader = stderr_pipe;
            reader.read_to_end(&mut buf).await.map(|_| buf)
        });

        let wait_result = timeout(timeout_duration, child.wait()).await;

        let status = match wait_result {
            Ok(Ok(exit_status)) => classify_status(exit_status),
            Ok(Err(e)) => return Err(ShellError::Io(format!("{e}"))),
            Err(_elapsed) => {
                // Kill on timeout; ignore the result since the process may have
                // already exited between the timeout firing and the kill call.
                let _ = child.kill().await;
                ExecStatus::Timeout
            }
        };

        // After wait/kill, the reader tasks reach EOF as the child's pipes close.
        let stdout = match stdout_task.await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(e)) => return Err(ShellError::Io(format!("{e}"))),
            Err(e) => return Err(ShellError::Io(format!("stdout reader task: {e}"))),
        };
        let stderr = match stderr_task.await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(e)) => return Err(ShellError::Io(format!("{e}"))),
            Err(e) => return Err(ShellError::Io(format!("stderr reader task: {e}"))),
        };

        Ok(ShellOutput {
            stdout,
            stderr,
            status,
        })
    }
}

/// Map an `ExitStatus` to the portable `ExecStatus`. On Unix, a process killed
/// by a signal reports `None` from `code()`; the signal branch covers that
/// case.
fn classify_status(exit_status: std::process::ExitStatus) -> ExecStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = exit_status.signal() {
            return ExecStatus::Signal {
                name: signal_name(signal),
            };
        }
    }
    ExecStatus::Exit {
        code: exit_status.code().unwrap_or(-1),
    }
}

#[cfg(unix)]
fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP".to_string(),
        2 => "SIGINT".to_string(),
        3 => "SIGQUIT".to_string(),
        9 => "SIGKILL".to_string(),
        15 => "SIGTERM".to_string(),
        other => format!("signal-{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::StubShell;

    #[tokio::test]
    async fn stub_shell_returns_scripted_exit() -> Result<(), Box<dyn std::error::Error>> {
        let stub = StubShell::new(vec![Ok(ShellOutput {
            stdout: b"hello\n".to_vec(),
            stderr: Vec::new(),
            status: ExecStatus::Exit { code: 0 },
        })]);

        let output = stub
            .exec("echo hello", Duration::from_secs(1))
            .await
            .expect("scripted exec succeeds");
        assert_eq!(output.stdout, b"hello\n");
        assert!(output.stderr.is_empty());
        match output.status {
            ExecStatus::Exit { code } => assert_eq!(code, 0),
            other => return Err(format!("expected Exit, got {other:?}").into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn stub_shell_returns_scripted_timeout() {
        let stub = StubShell::new(vec![Ok(ShellOutput {
            stdout: b"partial".to_vec(),
            stderr: b"warn".to_vec(),
            status: ExecStatus::Timeout,
        })]);

        let output = stub
            .exec("sleep 99", Duration::from_millis(10))
            .await
            .expect("scripted exec succeeds");
        assert!(matches!(output.status, ExecStatus::Timeout));
        assert_eq!(output.stdout, b"partial");
        assert_eq!(output.stderr, b"warn");
    }

    #[tokio::test]
    async fn stub_shell_propagates_scripted_error() -> Result<(), Box<dyn std::error::Error>> {
        let stub = StubShell::new(vec![Err(ShellError::Spawn("no /bin/bash".to_string()))]);

        let err = stub
            .exec("anything", Duration::from_secs(1))
            .await
            .expect_err("scripted error surfaces");
        match err {
            ShellError::Spawn(reason) => assert_eq!(reason, "no /bin/bash"),
            other => return Err(format!("expected Spawn, got {other:?}").into()),
        }
        Ok(())
    }

    /// Smoke test that actually spawns `/bin/bash`. Gated `#[ignore]` so the
    /// default test run does not depend on the host shell. Run explicitly with
    /// `cargo nextest run -- --ignored bash_shell_smoke`.
    #[tokio::test]
    #[ignore = "spawns real /bin/bash; opt-in"]
    async fn bash_shell_smoke_echoes_string() -> Result<(), Box<dyn std::error::Error>> {
        let shell = BashShell::new();
        let output = shell
            .exec("echo hello-from-bash", Duration::from_secs(5))
            .await
            .expect("bash exec succeeds");
        match output.status {
            ExecStatus::Exit { code } => assert_eq!(code, 0),
            other => return Err(format!("expected Exit {{code:0}}, got {other:?}").into()),
        }
        assert!(
            output.stdout.starts_with(b"hello-from-bash"),
            "stdout was {:?}",
            output.stdout
        );
        Ok(())
    }
}
