//! Model traits and provider submodules.

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

/// Structured input contract for a Model call. The Agent compiles its
/// History into this shape; provider adapters map it onto provider-native
/// payloads. The Model trait never sees `Block`.
#[derive(Clone, Debug)]
pub struct ModelInput {
    pub turns: Vec<Turn>,
}

#[derive(Clone, Debug)]
pub struct Turn {
    pub role: Role,
    pub content: Vec<ContentPart>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    System,
    User,
    Assistant,
}

/// A part of a Turn's content. Marked `#[non_exhaustive]` because future
/// phases will add `ToolUse` and `ToolResult` variants; downstream matchers
/// must remain forward-compatible.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ContentPart {
    Text(String),
}

pub trait ARModel: Send + Sync {
    fn complete<'a>(
        &'a self,
        input: &'a ModelInput,
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

    #[test]
    fn model_input_is_send_sync() {
        assert_send_sync::<ModelInput>();
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::User,
                content: vec![ContentPart::Text("hi".to_string())],
            }],
        };
        assert_eq!(input.turns.len(), 1);
        assert_eq!(input.turns[0].role, Role::User);
    }
}
