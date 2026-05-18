use std::path::PathBuf;

use serde_json::{Value, json};

use crate::builtins::path::{
    AtomicWriteError, PathResolveError, atomic_write, resolve_path as shared_resolve_path,
};
use crate::tool::{Tool, ToolError};

#[derive(serde::Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WriteError {
    InvalidPath { reason: String },
    PermissionDenied { path: String },
    IoError { reason: String },
}

fn err(e: WriteError) -> ToolError {
    let value =
        serde_json::to_value(e).unwrap_or_else(|_| serde_json::json!({"kind": "internal_error"}));
    ToolError::Execution(value)
}

#[derive(serde::Serialize)]
struct WriteOutput {
    bytes_written: u64,
}

fn resolve_path(input: &str) -> Result<PathBuf, ToolError> {
    shared_resolve_path(input).map_err(|e| match e {
        PathResolveError::Empty => err(WriteError::InvalidPath {
            reason: "path is empty".to_string(),
        }),
        PathResolveError::Cwd(msg) => err(WriteError::InvalidPath {
            reason: format!("cwd: {msg}"),
        }),
    })
}

pub struct Write {
    schema: Value,
}

impl Write {
    pub fn new() -> Self {
        Self {
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        }
    }
}

impl Default for Write {
    fn default() -> Self {
        Self::new()
    }
}

#[jsonrpsee::core::async_trait]
impl Tool for Write {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Atomically write `content` (UTF-8) to the file at `path`. Parent directories \
         are created as needed. Existing files are silently overwritten. \
         Returns `{bytes_written: u64}`."
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    async fn call(&self, input: Value) -> Result<Value, ToolError> {
        let parsed: WriteInput = serde_json::from_value(input).map_err(|e| {
            ToolError::InvalidInput(format!(
                "Write requires string fields 'path' and 'content': {e}"
            ))
        })?;

        let resolved = resolve_path(&parsed.path)?;

        if let Some(parent) = resolved.parent()
            && !parent.as_os_str().is_empty()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                return Err(err(WriteError::PermissionDenied {
                    path: resolved.display().to_string(),
                }));
            }
            return Err(err(WriteError::IoError {
                reason: format!("create_dir_all: {e}"),
            }));
        }

        atomic_write(&resolved, parsed.content.as_bytes())
            .await
            .map_err(|e| match e {
                AtomicWriteError::WriteTmp(msg) => {
                    if msg.to_ascii_lowercase().contains("permission denied") {
                        err(WriteError::PermissionDenied {
                            path: resolved.display().to_string(),
                        })
                    } else {
                        err(WriteError::IoError {
                            reason: format!("write tmp: {msg}"),
                        })
                    }
                }
                AtomicWriteError::Rename(msg) => {
                    if msg.to_ascii_lowercase().contains("permission denied") {
                        err(WriteError::PermissionDenied {
                            path: resolved.display().to_string(),
                        })
                    } else {
                        err(WriteError::IoError {
                            reason: format!("rename: {msg}"),
                        })
                    }
                }
            })?;

        let bytes_written = parsed.content.len() as u64;
        serde_json::to_value(WriteOutput { bytes_written }).map_err(|e| {
            err(WriteError::IoError {
                reason: format!("serialize output: {e}"),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn unique_tempdir() -> PathBuf {
        let id = Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join(format!("pristine-write-test-{id}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn execution_value(err: ToolError) -> Value {
        match err {
            ToolError::Execution(v) => v,
            other => panic!("expected ToolError::Execution, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_creates_file_with_content() {
        let dir = unique_tempdir();
        let path = dir.join("fresh.txt");
        let tool = Write::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "content": "hello",
            }))
            .await
            .expect("happy path returns Ok");

        assert_eq!(value["bytes_written"], 5);
        let on_disk = std::fs::read_to_string(&path).expect("read back file");
        assert_eq!(on_disk, "hello");
    }

    #[tokio::test]
    async fn write_overwrites_existing_file() {
        let dir = unique_tempdir();
        let path = dir.join("fixture.txt");
        std::fs::write(&path, b"old").expect("write fixture");
        let tool = Write::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "content": "new",
            }))
            .await
            .expect("overwrite returns Ok");

        assert_eq!(value["bytes_written"], 3);
        let on_disk = std::fs::read_to_string(&path).expect("read back file");
        assert_eq!(on_disk, "new");
    }

    #[tokio::test]
    async fn write_auto_creates_parent_directory() {
        let dir = unique_tempdir();
        let nested = dir.join("nested").join("dir").join("file.txt");
        let tool = Write::new();

        let value = tool
            .call(json!({
                "path": nested.to_string_lossy(),
                "content": "deep",
            }))
            .await
            .expect("nested write returns Ok");

        assert_eq!(value["bytes_written"], 4);
        assert!(nested.exists(), "target file exists");
        assert!(
            nested.parent().expect("has parent").is_dir(),
            "parent directory was created"
        );
        let on_disk = std::fs::read_to_string(&nested).expect("read back file");
        assert_eq!(on_disk, "deep");
    }

    #[tokio::test]
    async fn write_empty_content_creates_empty_file() {
        let dir = unique_tempdir();
        let path = dir.join("empty.txt");
        let tool = Write::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "content": "",
            }))
            .await
            .expect("empty write returns Ok");

        assert_eq!(value["bytes_written"], 0);
        assert!(path.exists(), "empty file exists");
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(metadata.len(), 0, "file is zero bytes");
    }

    #[tokio::test]
    async fn write_invalid_path_empty_string() {
        let tool = Write::new();

        let err = tool
            .call(json!({
                "path": "",
                "content": "anything",
            }))
            .await
            .expect_err("empty path must error");

        let value = execution_value(err);
        assert_eq!(value["kind"], "invalid_path");
        let reason = value["reason"].as_str().expect("reason is a string");
        assert!(!reason.is_empty(), "reason should be non-empty");
    }

    #[tokio::test]
    async fn write_handles_existing_directory_as_target() {
        // Pre-create a directory at the target path; the atomic-rename step
        // should fail because rename refuses to replace a non-empty directory
        // (and rename of a regular tmp file onto an existing directory is
        // rejected by the OS). We accept either io_error or permission_denied.
        let dir = unique_tempdir();
        let target = dir.join("collision");
        std::fs::create_dir_all(&target).expect("pre-create target dir");
        // Add a child so the directory is non-empty (avoids platform quirks
        // where renaming over an empty dir might succeed).
        std::fs::write(target.join("child.txt"), b"x").expect("populate dir");
        let tool = Write::new();

        let err = tool
            .call(json!({
                "path": target.to_string_lossy(),
                "content": "should not write",
            }))
            .await
            .expect_err("directory-as-target must error");

        let value = execution_value(err);
        let kind = value["kind"].as_str().expect("kind is a string");
        assert!(
            kind == "io_error" || kind == "permission_denied",
            "expected io_error or permission_denied, got {kind}"
        );
    }
}
