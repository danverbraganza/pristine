//! Shared crate-private path utilities for built-in tools.
//!
//! The portable resolution policy (absolute paths pass through; relative paths
//! are joined against the process cwd) lives here so Read, Write, Edit, and
//! Insert can share one implementation. Each tool maps the dialect-free
//! `PathResolveError` onto its own `InvalidPath { reason }` variant.

use std::path::PathBuf;

pub(crate) enum PathResolveError {
    Empty,
    Cwd(String),
}

pub(crate) fn resolve_path(input: &str) -> Result<PathBuf, PathResolveError> {
    if input.is_empty() {
        return Err(PathResolveError::Empty);
    }
    let p = std::path::Path::new(input);
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(|e| PathResolveError::Cwd(format!("{e}")))?;
    Ok(cwd.join(p))
}
