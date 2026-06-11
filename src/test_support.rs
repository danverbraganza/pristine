//! Shared fixtures for crate-level tests.
//!
//! Exposed as `pub` so integration tests under `tests/` can reach the
//! fixtures that have to round-trip through the crate's public API
//! (notably [`MapEnv`], a deterministic [`crate::config::EnvSource`]
//! implementation). Fixtures used only by `#[cfg(test)]` modules inside
//! `src/` remain `pub(crate)` and are per-item gated on `#[cfg(test)]`
//! so they do not produce dead-code warnings in non-test builds.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::{EnvSource, HomeSource};

#[cfg(test)]
use std::collections::VecDeque;
#[cfg(test)]
use std::path::Path;
#[cfg(test)]
use std::pin::Pin;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::time::Duration;

#[cfg(test)]
use futures::Stream;
#[cfg(test)]
use futures::stream;

#[cfg(test)]
use crate::model::{self, ARModel, ModelInput, ModelStreamEvent};
#[cfg(test)]
use crate::shell::{Shell, ShellError, ShellOutput};
#[cfg(test)]
use crate::tool::ToolError;

/// Creates a unique tempdir under `std::env::temp_dir()` for an individual
/// test. `prefix` namespaces the path to aid debugging when tests leave
/// artifacts behind.
#[cfg(test)]
pub(crate) fn unique_tempdir(prefix: &str) -> PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = std::env::temp_dir().join(format!("{prefix}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Writes `contents` to `dir/name` and returns the resulting path. Tests use
/// this to seed input fixtures into a tempdir prepared by `unique_tempdir`.
#[cfg(test)]
pub(crate) fn write_fixture(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, contents).expect("write fixture");
    p
}

/// Unwraps a `ToolError::Execution(value)` carrier, panicking on any other
/// variant. Built-in tools use the `Execution` carrier as the portable shape
/// for their dialect errors; this helper lets tests assert on the inner JSON
/// without restating the match.
#[cfg(test)]
pub(crate) fn execution_value(err: ToolError) -> serde_json::Value {
    match err {
        ToolError::Execution(v) => v,
        other => panic!("expected ToolError::Execution, got {other:?}"),
    }
}

#[cfg(test)]
pub(crate) struct StubArModel {
    scripts: Mutex<VecDeque<Vec<Result<ModelStreamEvent, model::Error>>>>,
    last_input: Mutex<Option<ModelInput>>,
}

#[cfg(test)]
impl StubArModel {
    pub fn with_call_scripts(scripts: Vec<Vec<Result<ModelStreamEvent, model::Error>>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            last_input: Mutex::new(None),
        }
    }

    pub fn with_events(events: Vec<Result<ModelStreamEvent, model::Error>>) -> Self {
        Self::with_call_scripts(vec![events])
    }

    pub fn empty() -> Self {
        Self::with_call_scripts(Vec::new())
    }

    pub fn last_input(&self) -> Option<ModelInput> {
        self.last_input.lock().expect("test lock").clone()
    }
}

#[cfg(test)]
impl ARModel for StubArModel {
    fn complete<'a>(
        &'a self,
        input: &'a ModelInput,
    ) -> Pin<Box<dyn Stream<Item = Result<ModelStreamEvent, model::Error>> + Send + 'a>> {
        *self.last_input.lock().expect("test lock") = Some(input.clone());
        let next = self
            .scripts
            .lock()
            .expect("test lock")
            .pop_front()
            .unwrap_or_default();
        Box::pin(stream::iter(next))
    }
}

#[cfg(test)]
pub(crate) struct EchoTool {
    name: String,
    description: String,
    schema: serde_json::Value,
}

#[cfg(test)]
impl EchoTool {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            description: "Echoes input back wrapped under `echo`.".to_string(),
            schema: serde_json::json!({ "type": "object" }),
        }
    }
}

#[cfg(test)]
#[jsonrpsee::core::async_trait]
impl crate::tool::Tool for EchoTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> &serde_json::Value {
        &self.schema
    }

    async fn call(
        &self,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, crate::tool::ToolError> {
        Ok(serde_json::json!({ "echo": input }))
    }
}

/// Test-only `Shell` fixture: pops scripted `Result<ShellOutput, ShellError>`
/// entries in order on each call. Mirrors `StubArModel`'s pattern. Also
/// records the most recent `timeout` argument so tests can assert on the
/// `ExecBash` default-timeout behavior.
#[cfg(test)]
pub(crate) struct StubShell {
    script: Mutex<VecDeque<Result<ShellOutput, ShellError>>>,
    last_timeout: Mutex<Option<Duration>>,
}

#[cfg(test)]
impl StubShell {
    pub fn new(script: Vec<Result<ShellOutput, ShellError>>) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            last_timeout: Mutex::new(None),
        }
    }

    pub(crate) fn last_timeout(&self) -> Option<Duration> {
        *self.last_timeout.lock().expect("test lock")
    }
}

#[cfg(test)]
#[jsonrpsee::core::async_trait]
impl Shell for StubShell {
    async fn exec(&self, command: &str, timeout: Duration) -> Result<ShellOutput, ShellError> {
        *self.last_timeout.lock().expect("test lock") = Some(timeout);
        let mut script = self.script.lock().expect("test lock");
        match script.pop_front() {
            Some(entry) => entry,
            None => Err(ShellError::Io(format!(
                "StubShell script exhausted at unexpected exec call: {command}"
            ))),
        }
    }
}

/// In-memory [`EnvSource`] for deterministic config tests. Reachable from
/// both `#[cfg(test)]` modules inside `src/` and integration tests under
/// `tests/`, so the hoisted fixture is `pub` (not `pub(crate)`) and is
/// not gated on `#[cfg(test)]`. `Default` matches the empty-env shape the
/// hoisted callers expect.
#[derive(Default)]
pub struct MapEnv(HashMap<String, String>);

impl MapEnv {
    pub fn new<const N: usize>(entries: [(&str, &str); N]) -> Self {
        Self(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }
}

impl EnvSource for MapEnv {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

/// In-memory [`HomeSource`] for deterministic config tests. Reachable from
/// both `#[cfg(test)]` modules inside `src/` and integration tests under
/// `tests/`, so the hoisted fixture is `pub` (not `pub(crate)`) and is not
/// gated on `#[cfg(test)]`. `Default` matches the `None`-home shape used by
/// callers that bypass home-dir lookup entirely.
#[derive(Default)]
pub struct MockHome(Option<PathBuf>);

impl MockHome {
    pub fn some(path: impl Into<PathBuf>) -> Self {
        Self(Some(path.into()))
    }

    pub fn none() -> Self {
        Self(None)
    }
}

impl HomeSource for MockHome {
    fn home_dir(&self) -> Option<PathBuf> {
        self.0.clone()
    }
}
