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

/// A part of a Turn's content; carries text and tool exchanges between the
/// Agent and the Model. Marked `#[non_exhaustive]` because additional
/// modalities (image, audio, etc.) are still anticipated and downstream
/// matchers must remain forward-compatible.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ContentPart {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: serde_json::Value,
        is_error: bool,
    },
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

    #[test]
    fn content_part_is_send_sync() {
        assert_send_sync::<ContentPart>();
    }

    #[test]
    fn tool_use_round_trips_through_model_input() {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse {
                    id: "call-1".to_string(),
                    name: "echo".to_string(),
                    input: serde_json::json!({ "hello": "world" }),
                }],
            }],
        };
        let part = &input.turns[0].content[0];
        match part {
            ContentPart::ToolUse { id, name, input } => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "echo");
                assert_eq!(input, &serde_json::json!({ "hello": "world" }));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_clone_preserves_fields() {
        let part = ContentPart::ToolResult {
            tool_use_id: "call-1".to_string(),
            content: serde_json::Value::Null,
            is_error: true,
        };
        let cloned = part.clone();
        match cloned {
            ContentPart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "call-1");
                assert_eq!(content, serde_json::Value::Null);
                assert!(is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }
}
