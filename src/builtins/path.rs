//! Shared crate-private path utilities for built-in tools.
//!
//! The portable resolution policy (absolute paths pass through; relative paths
//! are joined against the process cwd) lives here so Read, Write, Edit, and
//! Insert can share one implementation. Each tool maps the dialect-free
//! `PathResolveError` onto its own `InvalidPath { reason }` variant.
//!
//! `atomic_write` provides the write-then-rename pattern shared by the Edit,
//! Write, and Insert tools for crash-safe replacement of file contents. The
//! helper does not fsync the tmp file; callers that require durability beyond
//! the OS page cache must layer that on top.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

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

pub(crate) enum AtomicWriteError {
    WriteTmp(String),
    Rename(String),
}

pub(crate) async fn atomic_write(target: &Path, content: &[u8]) -> Result<(), AtomicWriteError> {
    let mut tmp_os: OsString = target.as_os_str().to_owned();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);
    tokio::fs::write(&tmp, content)
        .await
        .map_err(|e| AtomicWriteError::WriteTmp(format!("{e}")))?;
    tokio::fs::rename(&tmp, target)
        .await
        .map_err(|e| AtomicWriteError::Rename(format!("{e}")))?;
    Ok(())
}
