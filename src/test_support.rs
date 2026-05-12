//! Shared fixtures for crate-level tests.

use std::pin::Pin;
use std::sync::Mutex;

use futures::Stream;
use futures::stream;

use crate::history::Block;
use crate::model::{self, ARModel, ModelStreamEvent};

pub(crate) struct StubArModel {
    events: Mutex<Vec<Result<ModelStreamEvent, model::Error>>>,
}

impl StubArModel {
    pub fn with_events(events: Vec<Result<ModelStreamEvent, model::Error>>) -> Self {
        Self {
            events: Mutex::new(events),
        }
    }

    pub fn empty() -> Self {
        Self::with_events(Vec::new())
    }
}

impl ARModel for StubArModel {
    fn complete<'a>(
        &'a self,
        _system_prompt: &'a str,
        _messages: &'a [Block],
    ) -> Pin<Box<dyn Stream<Item = Result<ModelStreamEvent, model::Error>> + Send + 'a>> {
        let events = std::mem::take(&mut *self.events.lock().expect("test lock"));
        Box::pin(stream::iter(events))
    }
}
