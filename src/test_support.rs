//! Shared fixtures for crate-level tests.

use std::pin::Pin;
use std::sync::Mutex;

use futures::Stream;
use futures::stream;

use crate::model::{self, ARModel, ModelInput, ModelStreamEvent};

pub(crate) struct StubArModel {
    events: Mutex<Vec<Result<ModelStreamEvent, model::Error>>>,
    last_input: Mutex<Option<ModelInput>>,
}

impl StubArModel {
    pub fn with_events(events: Vec<Result<ModelStreamEvent, model::Error>>) -> Self {
        Self {
            events: Mutex::new(events),
            last_input: Mutex::new(None),
        }
    }

    pub fn empty() -> Self {
        Self::with_events(Vec::new())
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
        let events = std::mem::take(&mut *self.events.lock().expect("test lock"));
        Box::pin(stream::iter(events))
    }
}
