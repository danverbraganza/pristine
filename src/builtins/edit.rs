//! Edit: in-place `str_replace` with match-once safety. The atomic-rename
//! helper guarantees the file never reflects a partial write.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::builtins::path::{
    AtomicWriteError, PathResolveError, atomic_write, resolve_path as shared_resolve_path,
};
use crate::tool::{Tool, ToolError, execution_err};

#[derive(serde::Deserialize)]
struct EditInput {
    path: String,
    old_str: String,
    new_str: String,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EditError {
    MultipleMatches { count: u32 },
    NoMatches,
    FileNotFound { path: String },
    NotUtf8 { byte_offset: usize },
    InvalidPath { reason: String },
    IoError { reason: String },
}

#[derive(serde::Serialize)]
struct EditOutput {
    replaced: bool,
}

fn resolve_path(input: &str) -> Result<PathBuf, ToolError> {
    shared_resolve_path(input).map_err(|e| match e {
        PathResolveError::Empty => execution_err(EditError::InvalidPath {
            reason: "path is empty".to_string(),
        }),
        PathResolveError::Cwd(msg) => execution_err(EditError::InvalidPath {
            reason: format!("cwd: {msg}"),
        }),
    })
}

pub struct Edit {
    schema: Value,
}

impl Edit {
    pub fn new() -> Self {
        Self {
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_str": {"type": "string"},
                    "new_str": {"type": "string"}
                },
                "required": ["path", "old_str", "new_str"]
            }),
        }
    }
}

impl Default for Edit {
    fn default() -> Self {
        Self::new()
    }
}

#[jsonrpsee::core::async_trait]
impl Tool for Edit {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace a single occurrence of `old_str` with `new_str` in the file at `path`. \
         The match must be unique: zero matches or multiple matches produce a typed error. \
         When `old_str` equals `new_str`, the call is a no-op success."
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    async fn call(&self, input: Value) -> Result<Value, ToolError> {
        let parsed: EditInput = serde_json::from_value(input).map_err(|e| {
            ToolError::InvalidInput(format!(
                "Edit requires string fields 'path', 'old_str', 'new_str': {e}"
            ))
        })?;

        let resolved = resolve_path(&parsed.path)?;

        // NOTE: Empty `old_str` is treated as `NoMatches` rather than
        // `MultipleMatches`. `"".matches("")` is unbounded in spirit, but
        // user intent for "replace nothing" is degenerate; rejecting as
        // NoMatches is the clearer semantic.
        if parsed.old_str.is_empty() {
            return Err(execution_err(EditError::NoMatches));
        }

        let bytes = match tokio::fs::read(&resolved).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(execution_err(EditError::FileNotFound {
                    path: resolved.to_string_lossy().into_owned(),
                }));
            }
            Err(e) => {
                return Err(execution_err(EditError::IoError {
                    reason: format!("{e}"),
                }));
            }
        };

        let content = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(e) => {
                return Err(execution_err(EditError::NotUtf8 {
                    byte_offset: e.valid_up_to(),
                }));
            }
        };

        let count = content.matches(parsed.old_str.as_str()).count();
        if count == 0 {
            return Err(execution_err(EditError::NoMatches));
        }
        if count >= 2 {
            let count_u32 = u32::try_from(count).unwrap_or(u32::MAX);
            return Err(execution_err(EditError::MultipleMatches {
                count: count_u32,
            }));
        }

        if parsed.old_str == parsed.new_str {
            return Ok(serde_json::to_value(EditOutput { replaced: true })
                .unwrap_or_else(|_| json!({"replaced": true})));
        }

        let new_content = content.replacen(&parsed.old_str, &parsed.new_str, 1);

        atomic_write(&resolved, new_content.as_bytes())
            .await
            .map_err(|e| match e {
                AtomicWriteError::WriteTmp(msg) => execution_err(EditError::IoError {
                    reason: format!("write tmp: {msg}"),
                }),
                AtomicWriteError::Rename(msg) => execution_err(EditError::IoError {
                    reason: format!("rename: {msg}"),
                }),
            })?;

        Ok(serde_json::to_value(EditOutput { replaced: true })
            .unwrap_or_else(|_| json!({"replaced": true})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{execution_value, write_fixture};
    use uuid::Uuid;

    fn unique_tempdir() -> PathBuf {
        crate::test_support::unique_tempdir("pristine-edit-test")
    }

    #[tokio::test]
    async fn edit_replaces_single_match_writes_file_atomically() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"foo bar baz");
        let tool = Edit::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "old_str": "bar",
                "new_str": "BAR",
            }))
            .await
            .expect("happy path returns Ok");

        assert_eq!(value["replaced"], true);
        let on_disk = std::fs::read_to_string(&path).expect("read back fixture");
        assert_eq!(on_disk, "foo BAR baz");
    }

    #[tokio::test]
    async fn edit_returns_multiple_matches_on_count_ge_2() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"foo foo foo");
        let tool = Edit::new();

        let err = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "old_str": "foo",
                "new_str": "FOO",
            }))
            .await
            .expect_err("multiple matches must error");

        let value = execution_value(err)?;
        assert_eq!(value["kind"], "multiple_matches");
        assert_eq!(value["count"], 3);
        let on_disk = std::fs::read_to_string(&path).expect("read back fixture");
        assert_eq!(on_disk, "foo foo foo");
        Ok(())
    }

    #[tokio::test]
    async fn edit_returns_no_matches_on_zero() -> Result<(), Box<dyn std::error::Error>> {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"hello");
        let tool = Edit::new();

        let err = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "old_str": "xyz",
                "new_str": "...",
            }))
            .await
            .expect_err("zero matches must error");

        let value = execution_value(err)?;
        assert_eq!(value["kind"], "no_matches");
        let on_disk = std::fs::read_to_string(&path).expect("read back fixture");
        assert_eq!(on_disk, "hello");
        Ok(())
    }

    #[tokio::test]
    async fn edit_returns_no_matches_on_empty_old_str() -> Result<(), Box<dyn std::error::Error>> {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"hello");
        let tool = Edit::new();

        let err = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "old_str": "",
                "new_str": "x",
            }))
            .await
            .expect_err("empty old_str is treated as no_matches");

        let value = execution_value(err)?;
        assert_eq!(value["kind"], "no_matches");
        let on_disk = std::fs::read_to_string(&path).expect("read back fixture");
        assert_eq!(on_disk, "hello");
        Ok(())
    }

    #[tokio::test]
    async fn edit_returns_file_not_found_on_missing_path() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = unique_tempdir();
        let missing = dir.join(format!("nonexistent-{}.txt", Uuid::new_v4().simple()));
        let tool = Edit::new();

        let err = tool
            .call(json!({
                "path": missing.to_string_lossy(),
                "old_str": "x",
                "new_str": "y",
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
    async fn edit_returns_not_utf8_for_binary_file() -> Result<(), Box<dyn std::error::Error>> {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "binary.bin", &[0x48, 0x69, 0xFF, 0x80]);
        let tool = Edit::new();

        let err = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "old_str": "anything",
                "new_str": "x",
            }))
            .await
            .expect_err("invalid UTF-8 file must error");

        let value = execution_value(err)?;
        assert_eq!(value["kind"], "not_utf8");
        assert_eq!(value["byte_offset"], 2);
        Ok(())
    }

    #[tokio::test]
    async fn edit_returns_invalid_path_on_empty_string() -> Result<(), Box<dyn std::error::Error>> {
        let tool = Edit::new();

        let err = tool
            .call(json!({
                "path": "",
                "old_str": "x",
                "new_str": "y",
            }))
            .await
            .expect_err("empty path must error");

        let value = execution_value(err)?;
        assert_eq!(value["kind"], "invalid_path");
        let reason = value["reason"].as_str().expect("reason is a string");
        assert!(!reason.is_empty(), "reason should be non-empty");
        Ok(())
    }

    #[tokio::test]
    async fn edit_old_eq_new_is_noop_success() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"hello");
        let tool = Edit::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "old_str": "hello",
                "new_str": "hello",
            }))
            .await
            .expect("no-op returns Ok");

        assert_eq!(value["replaced"], true);
        let on_disk = std::fs::read_to_string(&path).expect("read back fixture");
        assert_eq!(on_disk, "hello");
    }

    #[tokio::test]
    async fn edit_preserves_trailing_newline() {
        let dir = unique_tempdir();
        let path = write_fixture(&dir, "fixture.txt", b"hello\n");
        let tool = Edit::new();

        let value = tool
            .call(json!({
                "path": path.to_string_lossy(),
                "old_str": "hello",
                "new_str": "HELLO",
            }))
            .await
            .expect("trailing newline preservation returns Ok");

        assert_eq!(value["replaced"], true);
        let on_disk = std::fs::read_to_string(&path).expect("read back fixture");
        assert_eq!(on_disk, "HELLO\n");
    }

    #[tokio::test]
    async fn edit_returns_invalid_input_on_malformed_json() -> Result<(), Box<dyn std::error::Error>>
    {
        let tool = Edit::new();

        let err = tool
            .call(json!({"path": "x"}))
            .await
            .expect_err("missing old_str/new_str must error");

        match err {
            ToolError::InvalidInput(_) => {}
            other => return Err(format!("expected InvalidInput, got {other:?}").into()),
        }
        Ok(())
    }
}
