//! Model traits and provider submodules.
//! `Usage` is a tuple variant on `ModelStreamEvent` so it shares the struct shape with `MessageComplete { usage: Usage }` without duplication.

pub mod anthropic;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModelRole {
    Default,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Clone, Debug)]
pub enum ModelStreamEvent {
    MessageStart { message_id: String, model: String },
    ContentDelta { text: String },
    ContentComplete { text: String },
    Usage(Usage),
    MessageComplete { usage: Usage },
    Error { message: String },
}

#[derive(Debug)]
pub enum Error {
    Http(String),
    Deserialization(String),
    Api { status: u16, message: String },
    Configuration(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Http(msg) => write!(f, "http error: {msg}"),
            Error::Deserialization(msg) => write!(f, "deserialization error: {msg}"),
            Error::Api { status, message } => write!(f, "api error (status {status}): {message}"),
            Error::Configuration(msg) => write!(f, "configuration error: {msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

/// `system_prompt` is passed explicitly because the Agent owns it (see plan.md §"Agent configuration").
pub trait ARModel: Send + Sync {
    fn complete<'a>(
        &'a self,
        system_prompt: &'a str,
        messages: &'a [crate::history::Block],
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<ModelStreamEvent, Error>> + Send + 'a>>;
}

/// Placeholder for future diffusion-language-model implementations.
pub trait DLModel: Send + Sync {}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_std_error<T: std::error::Error + Send + Sync + 'static>() {}

    #[test]
    fn model_stream_event_is_send_sync() {
        assert_send_sync::<ModelStreamEvent>();
    }

    #[test]
    fn error_is_standard_error_trait_object() {
        assert_send_sync::<Error>();
        assert_std_error::<Error>();
    }
}
