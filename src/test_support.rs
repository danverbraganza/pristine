//! Shared fixtures for crate-level tests.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Mutex;

use futures::Stream;
use futures::stream;

use crate::model::{self, ARModel, ModelInput, ModelStreamEvent};

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
