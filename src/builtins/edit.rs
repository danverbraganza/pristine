use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::tool::{Tool, ToolError};

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

fn err(e: EditError) -> ToolError {
    let value =
        serde_json::to_value(e).unwrap_or_else(|_| serde_json::json!({"kind": "internal_error"}));
    ToolError::Execution(value)
}

#[derive(serde::Serialize)]
struct EditOutput {
    replaced: bool,
}

fn resolve_path(input: &str) -> Result<PathBuf, ToolError> {
    if input.is_empty() {
        return Err(err(EditError::InvalidPath {
            reason: "path is empty".to_string(),
        }));
    }
    let p = Path::new(input);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else {
        let cwd = std::env::current_dir().map_err(|e| {
            err(EditError::InvalidPath {
                reason: format!("cwd: {e}"),
            })
        })?;
        cwd.join(p)
    };
    Ok(resolved)
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
            return Err(err(EditError::NoMatches));
        }

        let bytes = match tokio::fs::read(&resolved).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(err(EditError::FileNotFound {
                    path: resolved.to_string_lossy().into_owned(),
                }));
            }
            Err(e) => {
                return Err(err(EditError::IoError {
                    reason: format!("{e}"),
                }));
            }
        };

        let content = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(e) => {
                return Err(err(EditError::NotUtf8 {
                    byte_offset: e.valid_up_to(),
                }));
            }
        };

        let count = content.matches(parsed.old_str.as_str()).count();
        if count == 0 {
            return Err(err(EditError::NoMatches));
        }
        if count >= 2 {
            let count_u32 = u32::try_from(count).unwrap_or(u32::MAX);
            return Err(err(EditError::MultipleMatches { count: count_u32 }));
        }

        if parsed.old_str == parsed.new_str {
            return Ok(serde_json::to_value(EditOutput { replaced: true })
                .unwrap_or_else(|_| json!({"replaced": true})));
        }

        let new_content = content.replacen(&parsed.old_str, &parsed.new_str, 1);

        let mut tmp_path = resolved.clone().into_os_string();
        tmp_path.push(".tmp");
        let tmp_path = PathBuf::from(tmp_path);

        if let Err(e) = tokio::fs::write(&tmp_path, new_content.as_bytes()).await {
            return Err(err(EditError::IoError {
                reason: format!("write tmp: {e}"),
            }));
        }
        if let Err(e) = tokio::fs::rename(&tmp_path, &resolved).await {
            return Err(err(EditError::IoError {
                reason: format!("rename: {e}"),
            }));
        }

        Ok(serde_json::to_value(EditOutput { replaced: true })
            .unwrap_or_else(|_| json!({"replaced": true})))
    }
}
