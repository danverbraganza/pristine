//! Insert: inserts text at a 1-indexed line position in a UTF-8 text file.
//! `after_line == 0` prepends, `after_line == total_lines` appends, and the
//! file write goes through the shared atomic-rename helper.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::builtins::path::{AtomicWriteError, atomic_write, resolve_path as shared_resolve_path};
use crate::tool::{Tool, ToolError, execution_err};

#[derive(serde::Deserialize)]
struct InsertInput {
    path: String,
    after_line: usize,
    content: String,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum InsertError {
    FileNotFound {
        path: String,
    },
    NotUtf8 {
        byte_offset: usize,
    },
    InvalidAfterLine {
        after_line: usize,
        total_lines: usize,
    },
    InvalidPath {
        reason: String,
    },
    IoError {
        reason: String,
    },
}

#[derive(serde::Serialize)]
struct InsertOutput {
    lines_inserted: usize,
}

fn resolve_path(input: &str) -> Result<PathBuf, ToolError> {
    shared_resolve_path(input).map_err(|e| {
        execution_err(InsertError::InvalidPath {
            reason: e.to_string(),
        })
    })
}

/// Number of logical lines in `s`: count of `\n` plus 1 if `s` is non-empty
/// and does not end with `\n`. Matches the user-intuitive line count, e.g.
/// `"a"` -> 1, `"a\n"` -> 1, `"a\nb"` -> 2, `"a\nb\n"` -> 2.
fn logical_line_count(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    let nl = s.bytes().filter(|b| *b == b'\n').count();
    if s.ends_with('\n') { nl } else { nl + 1 }
}

pub struct Insert {
    schema: Value,
}

impl Insert {
    pub fn new() -> Self {
        Self {
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "after_line": {"type": "integer", "minimum": 0},
                    "content": {"type": "string"}
                },
                "required": ["path", "after_line", "content"]
            }),
        }
    }
}

impl Default for Insert {
    fn default() -> Self {
        Self::new()
    }
}

#[jsonrpsee::core::async_trait]
impl Tool for Insert {
    fn name(&self) -> &str {
        "insert"
    }

    fn description(&self) -> &str {
        "Insert `content` at line position `after_line` in the UTF-8 text file \
         at `path`. `after_line` is 1-indexed in the intuitive sense: \
         `after_line == 0` prepends, `after_line == total_lines` appends, and \
         intermediate values insert between line `after_line` and line \
         `after_line + 1`. Empty `content` is a no-op success. Returns \
         `{lines_inserted: usize}`."
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    async fn call(&self, input: Value) -> Result<Value, ToolError> {
        let parsed: InsertInput = serde_json::from_value(input).map_err(|e| {
            ToolError::InvalidInput(format!(
                "Insert requires string fields 'path', 'content' and integer 'after_line': {e}"
            ))
        })?;

        let resolved = resolve_path(&parsed.path)?;

        let bytes = match tokio::fs::read(&resolved).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(execution_err(InsertError::FileNotFound {
                    path: resolved.to_string_lossy().into_owned(),
                }));
            }
            Err(e) => {
                return Err(execution_err(InsertError::IoError {
                    reason: format!("{e}"),
                }));
            }
        };

        let original = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(e) => {
                return Err(execution_err(InsertError::NotUtf8 {
                    byte_offset: e.valid_up_to(),
                }));
            }
        };

        let total_lines = logical_line_count(original);

        if parsed.after_line > total_lines {
            return Err(execution_err(InsertError::InvalidAfterLine {
                after_line: parsed.after_line,
                total_lines,
            }));
        }

        // NOTE: Empty content is a no-op success: we skip the atomic write
        // entirely to avoid spurious tmp-file traffic for a write that would
        // not change the file's contents.
        if parsed.content.is_empty() {
            return serde_json::to_value(InsertOutput { lines_inserted: 0 }).map_err(|e| {
                execution_err(InsertError::IoError {
                    reason: format!("serialize output: {e}"),
                })
            });
        }

        // Decompose the original into logical lines. The `lines` vec has
        // exactly `total_lines` entries, each one a single line body without
        // the trailing newline.
        let lines: Vec<&str> = if original.is_empty() {
            Vec::new()
        } else if let Some(stripped) = original.strip_suffix('\n') {
            stripped.split('\n').collect()
        } else {
            original.split('\n').collect()
        };

        // Normalize the inserted content into lines via the same scheme so the
        // splice point is unambiguous.
        let insert_body: Vec<&str> = if let Some(stripped) = parsed.content.strip_suffix('\n') {
            stripped.split('\n').collect()
        } else {
            parsed.content.split('\n').collect()
        };

        let lines_inserted = logical_line_count(&parsed.content);

        let mut out_lines: Vec<&str> = Vec::with_capacity(lines.len() + insert_body.len());
        out_lines.extend_from_slice(&lines[..parsed.after_line]);
        out_lines.extend_from_slice(&insert_body);
        out_lines.extend_from_slice(&lines[parsed.after_line..]);

        // Re-join with `\n` and always terminate with `\n` whenever there is
        // any content. This keeps the file well-formed for mid-file inserts
        // and, when appending, adds the separating newline between the
        // original last line and the inserted body (whether or not the
        // original ended in `\n`).
        let mut new_content = out_lines.join("\n");
        if !new_content.is_empty() {
            new_content.push('\n');
        }

        atomic_write(&resolved, new_content.as_bytes())
            .await
            .map_err(|e| match e {
                AtomicWriteError::WriteTmp(msg) => execution_err(InsertError::IoError {
                    reason: format!("write tmp: {msg}"),
                }),
                AtomicWriteError::Rename(msg) => execution_err(InsertError::IoError {
                    reason: format!("rename: {msg}"),
                }),
            })?;

        serde_json::to_value(InsertOutput { lines_inserted }).map_err(|e| {
            execution_err(InsertError::IoError {
                reason: format!("serialize output: {e}"),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{execution_value, write_fixture};
    use uuid::Uuid;

    fn unique_tempdir() -> PathBuf {
        crate::test_support::unique_tempdir("pristine-insert-test")
    }

    #[tokio::test]
    async fn insert_in_middle_of_file() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"a\nb\nc\n");
        let tool = Insert::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "after_line": 1,
                "content": "X\n",
            }))
            .await
            .expect("happy path returns Ok");

        assert_eq!(value["lines_inserted"], 1);
        let on_disk = std::fs::read_to_string(&path).expect("read back fixture");
        assert_eq!(on_disk, "a\nX\nb\nc\n");
    }

    #[tokio::test]
    async fn insert_prepends_when_after_line_zero() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"a\nb\n");
        let tool = Insert::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "after_line": 0,
                "content": "X",
            }))
            .await
            .expect("prepend returns Ok");

        assert_eq!(value["lines_inserted"], 1);
        let on_disk = std::fs::read_to_string(&path).expect("read back fixture");
        assert_eq!(on_disk, "X\na\nb\n");
    }

    #[tokio::test]
    async fn insert_appends_when_after_line_equals_total() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"a\nb\n");
        let tool = Insert::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "after_line": 2,
                "content": "X",
            }))
            .await
            .expect("append returns Ok");

        assert_eq!(value["lines_inserted"], 1);
        let on_disk = std::fs::read_to_string(&path).expect("read back fixture");
        assert_eq!(on_disk, "a\nb\nX\n");
    }

    #[tokio::test]
    async fn insert_returns_invalid_after_line_when_past_end()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"a\nb\n");
        let tool = Insert::new();

        let err = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "after_line": 99,
                "content": "X",
            }))
            .await
            .expect_err("after_line past end must error");

        let value = execution_value(err)?;
        assert_eq!(value["kind"], "invalid_after_line");
        assert_eq!(value["after_line"], 99);
        assert_eq!(value["total_lines"], 2);
        let on_disk = std::fs::read_to_string(&path).expect("read back fixture");
        assert_eq!(on_disk, "a\nb\n");
        Ok(())
    }

    #[tokio::test]
    async fn insert_empty_content_is_noop_success() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"a\n");
        let tool = Insert::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "after_line": 1,
                "content": "",
            }))
            .await
            .expect("empty content returns Ok");

        assert_eq!(value["lines_inserted"], 0);
        let on_disk = std::fs::read_to_string(&path).expect("read back fixture");
        assert_eq!(on_disk, "a\n");
    }

    #[tokio::test]
    async fn insert_multiline_content() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"a\nb\n");
        let tool = Insert::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "after_line": 1,
                "content": "X\nY",
            }))
            .await
            .expect("multiline insert returns Ok");

        assert_eq!(value["lines_inserted"], 2);
        let on_disk = std::fs::read_to_string(&path).expect("read back fixture");
        assert_eq!(on_disk, "a\nX\nY\nb\n");
    }

    #[tokio::test]
    async fn insert_into_empty_file() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "empty.txt", b"");
        let tool = Insert::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "after_line": 0,
                "content": "X",
            }))
            .await
            .expect("insert into empty file returns Ok");

        assert_eq!(value["lines_inserted"], 1);
        let on_disk = std::fs::read_to_string(&path).expect("read back fixture");
        assert_eq!(on_disk, "X\n");
    }

    #[tokio::test]
    async fn insert_append_to_file_without_trailing_newline() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"a");
        let tool = Insert::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "after_line": 1,
                "content": "X",
            }))
            .await
            .expect("append to no-trailing-newline file returns Ok");

        assert_eq!(value["lines_inserted"], 1);
        let on_disk = std::fs::read_to_string(&path).expect("read back fixture");
        assert_eq!(on_disk, "a\nX\n");
    }

    #[tokio::test]
    async fn insert_not_utf8() -> Result<(), Box<dyn std::error::Error>> {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "binary.bin", &[0x48, 0x69, 0xFF, 0x80]);
        let tool = Insert::new();

        let err = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "after_line": 0,
                "content": "X",
            }))
            .await
            .expect_err("invalid UTF-8 file must error");

        let value = execution_value(err)?;
        assert_eq!(value["kind"], "not_utf8");
        assert_eq!(value["byte_offset"], 2);
        Ok(())
    }

    #[tokio::test]
    async fn insert_file_not_found() -> Result<(), Box<dyn std::error::Error>> {
        let dir = unique_tempdir();
        let missing = dir.join(format!("nonexistent-{}.txt", Uuid::new_v4().simple()));
        let tool = Insert::new();

        let err = tool
            .call(json!({
                "path": missing.to_string_lossy(),
                "after_line": 0,
                "content": "X",
            }))
            .await
            .expect_err("missing path must error");

        let value = execution_value(err)?;
        assert_eq!(value["kind"], "file_not_found");
        assert_eq!(
            value["path"].as_str().expect("path is a string"),
            missing.to_string_lossy(),
        );
        Ok(())
    }

    #[tokio::test]
    async fn insert_invalid_path_empty_string() -> Result<(), Box<dyn std::error::Error>> {
        let tool = Insert::new();

        let err = tool
            .call(json!({
                "path": "",
                "after_line": 0,
                "content": "X",
            }))
            .await
            .expect_err("empty path must error");

        let value = execution_value(err)?;
        assert_eq!(value["kind"], "invalid_path");
        let reason = value["reason"].as_str().expect("reason is a string");
        assert!(!reason.is_empty(), "reason should be non-empty");
        Ok(())
    }
}
