use std::path::PathBuf;

use serde_json::{Value, json};

use crate::builtins::path::{PathResolveError, resolve_path as shared_resolve_path};
use crate::tool::{Tool, ToolError, execution_err};

const MAX_BYTES: u64 = 64 * 1024;

#[derive(serde::Deserialize)]
struct ReadInput {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReadError {
    FileNotFound { path: String },
    FileTooLarge { size_bytes: u64, max_bytes: u64 },
    NotUtf8 { byte_offset: usize },
    InvalidRange { start_line: usize, end_line: usize },
    InvalidPath { reason: String },
    IoError { reason: String },
}

#[derive(serde::Serialize)]
struct ReadOutput {
    content: String,
}

fn resolve_path(input: &str) -> Result<PathBuf, ToolError> {
    shared_resolve_path(input).map_err(|e| match e {
        PathResolveError::Empty => execution_err(ReadError::InvalidPath {
            reason: "path is empty".to_string(),
        }),
        PathResolveError::Cwd(msg) => execution_err(ReadError::InvalidPath {
            reason: format!("cwd: {msg}"),
        }),
    })
}

pub struct Read {
    schema: Value,
}

impl Read {
    pub fn new() -> Self {
        Self {
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1}
                },
                "required": ["path"]
            }),
        }
    }
}

impl Default for Read {
    fn default() -> Self {
        Self::new()
    }
}

#[jsonrpsee::core::async_trait]
impl Tool for Read {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text file. Optional 1-indexed inclusive `start_line` / `end_line` \
         range bounds the output and bypasses the 64 KiB whole-file size cap. \
         Returns `{content: String}`."
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    async fn call(&self, input: Value) -> Result<Value, ToolError> {
        let parsed: ReadInput = serde_json::from_value(input).map_err(|e| {
            ToolError::InvalidInput(format!(
                "Read requires string field 'path' and optional integer fields \
                 'start_line', 'end_line': {e}"
            ))
        })?;

        let resolved = resolve_path(&parsed.path)?;

        let has_range = parsed.start_line.is_some() || parsed.end_line.is_some();

        let metadata = match tokio::fs::metadata(&resolved).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(execution_err(ReadError::FileNotFound {
                    path: resolved.to_string_lossy().into_owned(),
                }));
            }
            Err(e) => {
                return Err(execution_err(ReadError::IoError {
                    reason: format!("{e}"),
                }));
            }
        };

        let size_bytes = metadata.len();
        if !has_range && size_bytes > MAX_BYTES {
            return Err(execution_err(ReadError::FileTooLarge {
                size_bytes,
                max_bytes: MAX_BYTES,
            }));
        }

        let bytes = match tokio::fs::read(&resolved).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(execution_err(ReadError::FileNotFound {
                    path: resolved.to_string_lossy().into_owned(),
                }));
            }
            Err(e) => {
                return Err(execution_err(ReadError::IoError {
                    reason: format!("{e}"),
                }));
            }
        };

        let content = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(e) => {
                return Err(execution_err(ReadError::NotUtf8 {
                    byte_offset: e.valid_up_to(),
                }));
            }
        };

        let out = if has_range {
            slice_lines(content, parsed.start_line, parsed.end_line)?
        } else {
            content.to_string()
        };

        serde_json::to_value(ReadOutput { content: out }).map_err(|e| {
            execution_err(ReadError::IoError {
                reason: format!("serialize output: {e}"),
            })
        })
    }
}

fn slice_lines(
    content: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<String, ToolError> {
    // `start_line == 0` is invalid (1-indexed).
    if let Some(0) = start_line {
        return Err(execution_err(ReadError::InvalidRange {
            start_line: 0,
            end_line: end_line.unwrap_or(0),
        }));
    }
    if let (Some(s), Some(e)) = (start_line, end_line)
        && s > e
    {
        return Err(execution_err(ReadError::InvalidRange {
            start_line: s,
            end_line: e,
        }));
    }

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    let start_1 = start_line.unwrap_or(1);
    let end_1 = end_line.unwrap_or(total);

    if start_1 > total {
        return Ok(String::new());
    }
    let end_clamped = end_1.min(total);

    let start_idx = start_1 - 1;
    let slice = &lines[start_idx..end_clamped];
    let mut out = slice.join("\n");

    // Preserve trailing newline when the slice includes the last line of a
    // file that originally ended with `\n`.
    let included_last = end_clamped == total && total > 0;
    if included_last && content.ends_with('\n') {
        out.push('\n');
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{execution_value, write_fixture};
    use uuid::Uuid;

    fn unique_tempdir() -> PathBuf {
        crate::test_support::unique_tempdir("pristine-read-test")
    }

    #[tokio::test]
    async fn read_full_file_happy_path() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"hello\nworld\n");
        let tool = Read::new();

        let value = tool
            .call(json!({"path": path.to_string_lossy()}))
            .await
            .expect("happy path returns Ok");

        assert_eq!(value["content"], "hello\nworld\n");
    }

    #[tokio::test]
    async fn read_empty_file_returns_empty_content() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "empty.txt", b"");
        let tool = Read::new();

        let value = tool
            .call(json!({"path": path.to_string_lossy()}))
            .await
            .expect("empty file read returns Ok");

        assert_eq!(value["content"], "");
    }

    #[tokio::test]
    async fn read_file_not_found_returns_typed_error() {
        let dir = unique_tempdir();
        let missing = dir.join(format!("nonexistent-{}.txt", Uuid::new_v4().simple()));
        let tool = Read::new();

        let err = tool
            .call(json!({"path": missing.to_string_lossy()}))
            .await
            .expect_err("missing path must error");

        let value = execution_value(err);
        assert_eq!(value["kind"], "file_not_found");
        assert_eq!(
            value["path"].as_str().expect("path is a string"),
            missing.to_string_lossy(),
        );
    }

    #[tokio::test]
    async fn read_file_too_large_when_no_range_given() {
        let dir = unique_tempdir();
        let big = vec![b'a'; 65 * 1024];
        let path = write_fixture(&dir, "big.txt", &big);
        let tool = Read::new();

        let err = tool
            .call(json!({"path": path.to_string_lossy()}))
            .await
            .expect_err("file exceeding cap must error");

        let value = execution_value(err);
        assert_eq!(value["kind"], "file_too_large");
        assert_eq!(value["size_bytes"], 66560);
        assert_eq!(value["max_bytes"], 65536);
    }

    #[tokio::test]
    async fn read_file_too_large_bypassed_by_line_range() {
        let dir = unique_tempdir();
        let big = vec![b'a'; 65 * 1024];
        let path = write_fixture(&dir, "big.txt", &big);
        let tool = Read::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "start_line": 1,
                "end_line": 1,
            }))
            .await
            .expect("range bypasses the cap");

        let content = value["content"].as_str().expect("content is a string");
        assert_eq!(content.len(), 65 * 1024);
        assert!(content.chars().all(|c| c == 'a'));
    }

    #[tokio::test]
    async fn read_not_utf8_returns_byte_offset() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "binary.bin", &[0x48, 0x69, 0xFF, 0x80]);
        let tool = Read::new();

        let err = tool
            .call(json!({"path": path.to_string_lossy()}))
            .await
            .expect_err("invalid UTF-8 must error");

        let value = execution_value(err);
        assert_eq!(value["kind"], "not_utf8");
        assert_eq!(value["byte_offset"], 2);
    }

    #[tokio::test]
    async fn read_invalid_range_start_zero() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"a\nb");
        let tool = Read::new();

        let err = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "start_line": 0,
                "end_line": 1,
            }))
            .await
            .expect_err("start_line == 0 must error");

        let value = execution_value(err);
        assert_eq!(value["kind"], "invalid_range");
    }

    #[tokio::test]
    async fn read_invalid_range_start_gt_end() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"a\nb\nc");
        let tool = Read::new();

        let err = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "start_line": 2,
                "end_line": 1,
            }))
            .await
            .expect_err("start > end must error");

        let value = execution_value(err);
        assert_eq!(value["kind"], "invalid_range");
        assert_eq!(value["start_line"], 2);
        assert_eq!(value["end_line"], 1);
    }

    #[tokio::test]
    async fn read_range_clamps_end_past_total() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"a\nb");
        let tool = Read::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "start_line": 1,
                "end_line": 99,
            }))
            .await
            .expect("end past total clamps, not errors");

        assert_eq!(value["content"], "a\nb");
    }

    #[tokio::test]
    async fn read_start_past_total_returns_empty_content() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"a\n");
        let tool = Read::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "start_line": 99,
                "end_line": 100,
            }))
            .await
            .expect("start past total is empty, not an error");

        assert_eq!(value["content"], "");
    }

    #[tokio::test]
    async fn read_invalid_path_empty_string() {
        let tool = Read::new();

        let err = tool
            .call(json!({"path": ""}))
            .await
            .expect_err("empty path must error");

        let value = execution_value(err);
        assert_eq!(value["kind"], "invalid_path");
        let reason = value["reason"].as_str().expect("reason is a string");
        assert!(!reason.is_empty(), "reason should be non-empty");
    }
}
