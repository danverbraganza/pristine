//! Shared fixtures for crate-level tests.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use futures::Stream;
use futures::stream;

use crate::model::{self, ARModel, ModelInput, ModelStreamEvent};
use crate::shell::{Shell, ShellError, ShellOutput};

pub(crate) struct StubArModel {
    scripts: Mutex<VecDeque<Vec<Result<ModelStreamEvent, model::Error>>>>,
    last_input: Mutex<Option<ModelInput>>,
}

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

pub(crate) struct EchoTool {
    name: String,
    description: String,
    schema: serde_json::Value,
}

impl EchoTool {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            description: "Echoes input back wrapped under `echo`.".to_string(),
            schema: serde_json::json!({ "type": "object" }),
        }
    }
}

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
pub(crate) struct StubShell {
    script: Mutex<VecDeque<Result<ShellOutput, ShellError>>>,
    last_timeout: Mutex<Option<Duration>>,
}

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

#[jsonrpsee::core::async_trait]
impl Shell for StubShell {
    async fn exec(&self, command: &str, timeout: Duration) -> Result<ShellOutput, ShellError> {
        *self.last_timeout.lock().expect("test lock") = Some(timeout);
        let mut script = self.script.lock().expect("test lock");
        match script.pop_front() {
            Some(entry) => entry,
            None => panic!("StubShell script exhausted at unexpected exec call: {command}"),
        }
    }
}
