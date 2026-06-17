//! Model traits and provider submodules.

pub mod anthropic;
pub mod deepseek;
mod openai_dialect;
pub mod openrouter;

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
    MessageStart {
        message_id: String,
        model: String,
    },
    ContentDelta {
        text: String,
    },
    ContentComplete {
        text: String,
    },
    /// Incremental chunk of the model's reasoning/thinking output. Consumers
    /// accumulate deltas until the matching `ReasoningComplete`.
    ReasoningDelta {
        text: String,
    },
    /// The model's reasoning/thinking output is complete. `text` is the full
    /// accumulated reasoning trace.
    ReasoningComplete {
        text: String,
    },
    Usage(Usage),
    MessageComplete {
        usage: Usage,
    },
    Error {
        message: String,
    },
    /// Provider opened a tool_use content block. `id` is the provider-issued
    /// tool-use identifier; `name` is the tool the provider wants invoked.
    ToolUseStart {
        id: String,
        name: String,
    },
    /// Incremental JSON-input fragment for an in-flight tool_use. Consumers
    /// accumulate fragments until the matching `ToolUseComplete`; `id`
    /// disambiguates interleaved tool_uses.
    ToolUseDelta {
        id: String,
        partial_json: String,
    },
    /// Provider closed the tool_use block. `input` is the fully-parsed JSON;
    /// the adapter is responsible for accumulating partial deltas and
    /// deserializing before emitting this event.
    ToolUseComplete {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug)]
pub enum Error {
    Http(String),
    Deserialization(String),
    Api { status: u16, message: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Http(msg) => write!(f, "http error: {msg}"),
            Error::Deserialization(msg) => write!(f, "deserialization error: {msg}"),
            Error::Api { status, message } => write!(f, "api error (status {status}): {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Http(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Deserialization(e.to_string())
    }
}

/// Provider-agnostic descriptor for a tool the model may invoke. Carries no
/// behavior; provider adapters translate this into their own payload types.
#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Structured input contract for a Model call. The Agent compiles its
/// History into this shape; provider adapters map it onto provider-native
/// payloads. The Model trait never sees `Block`.
#[derive(Clone, Debug)]
pub struct ModelInput {
    pub turns: Vec<Turn>,
    pub tools: Vec<ToolSpec>,
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
            tools: Vec::new(),
        };
        assert_eq!(input.turns.len(), 1);
        assert_eq!(input.turns[0].role, Role::User);
    }

    #[test]
    fn content_part_is_send_sync() {
        assert_send_sync::<ContentPart>();
    }

    #[test]
    fn tool_use_round_trips_through_model_input() -> Result<(), Box<dyn std::error::Error>> {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse {
                    id: "call-1".to_string(),
                    name: "echo".to_string(),
                    input: serde_json::json!({ "hello": "world" }),
                }],
            }],
            tools: Vec::new(),
        };
        let part = &input.turns[0].content[0];
        match part {
            ContentPart::ToolUse { id, name, input } => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "echo");
                assert_eq!(input, &serde_json::json!({ "hello": "world" }));
            }
            other => return Err(format!("expected ToolUse, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn tool_result_clone_preserves_fields() -> Result<(), Box<dyn std::error::Error>> {
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
            other => return Err(format!("expected ToolResult, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn tool_use_stream_events_round_trip_their_fields() -> Result<(), Box<dyn std::error::Error>> {
        let start = ModelStreamEvent::ToolUseStart {
            id: "tu-1".to_string(),
            name: "echo".to_string(),
        };
        match start {
            ModelStreamEvent::ToolUseStart { id, name } => {
                assert_eq!(id, "tu-1");
                assert_eq!(name, "echo");
            }
            other => return Err(format!("expected ToolUseStart, got {other:?}").into()),
        }

        let delta = ModelStreamEvent::ToolUseDelta {
            id: "tu-1".to_string(),
            partial_json: "{\"k\":".to_string(),
        };
        match delta {
            ModelStreamEvent::ToolUseDelta { id, partial_json } => {
                assert_eq!(id, "tu-1");
                assert_eq!(partial_json, "{\"k\":");
            }
            other => return Err(format!("expected ToolUseDelta, got {other:?}").into()),
        }

        let complete = ModelStreamEvent::ToolUseComplete {
            id: "tu-1".to_string(),
            name: "echo".to_string(),
            input: serde_json::json!({ "k": "v" }),
        };
        match complete {
            ModelStreamEvent::ToolUseComplete { id, name, input } => {
                assert_eq!(id, "tu-1");
                assert_eq!(name, "echo");
                assert_eq!(input, serde_json::json!({ "k": "v" }));
            }
            other => return Err(format!("expected ToolUseComplete, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn reasoning_stream_events_round_trip_their_fields() -> Result<(), Box<dyn std::error::Error>> {
        let delta = ModelStreamEvent::ReasoningDelta {
            text: "let me think".to_string(),
        };
        match delta {
            ModelStreamEvent::ReasoningDelta { text } => assert_eq!(text, "let me think"),
            other => return Err(format!("expected ReasoningDelta, got {other:?}").into()),
        }

        let complete = ModelStreamEvent::ReasoningComplete {
            text: "let me think about it".to_string(),
        };
        match complete {
            ModelStreamEvent::ReasoningComplete { text } => {
                assert_eq!(text, "let me think about it")
            }
            other => return Err(format!("expected ReasoningComplete, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn tool_use_delta_distinguishes_interleaved_ids() -> Result<(), Box<dyn std::error::Error>> {
        let a = ModelStreamEvent::ToolUseDelta {
            id: "tu-a".to_string(),
            partial_json: "{\"a\":1}".to_string(),
        };
        let b = ModelStreamEvent::ToolUseDelta {
            id: "tu-b".to_string(),
            partial_json: "{\"b\":2}".to_string(),
        };
        let id_a = match a {
            ModelStreamEvent::ToolUseDelta { id, .. } => id,
            other => return Err(format!("expected ToolUseDelta, got {other:?}").into()),
        };
        let id_b = match b {
            ModelStreamEvent::ToolUseDelta { id, .. } => id,
            other => return Err(format!("expected ToolUseDelta, got {other:?}").into()),
        };
        assert_ne!(id_a, id_b);
        Ok(())
    }

    #[test]
    fn tool_spec_fields_round_trip() {
        let spec = ToolSpec {
            name: "echo".to_string(),
            description: "Echoes input".to_string(),
            input_schema: serde_json::json!({ "type": "object" }),
        };
        assert_eq!(spec.name, "echo");
        assert_eq!(spec.description, "Echoes input");
        assert_eq!(spec.input_schema, serde_json::json!({ "type": "object" }));
    }

    #[test]
    fn model_input_carries_tools_alongside_turns() {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::User,
                content: vec![ContentPart::Text("hi".into())],
            }],
            tools: vec![ToolSpec {
                name: "echo".into(),
                description: "".into(),
                input_schema: serde_json::Value::Null,
            }],
        };
        assert_eq!(input.tools.len(), 1);
        assert_eq!(input.turns.len(), 1);
        assert_eq!(input.tools[0].name, "echo");
    }

    #[test]
    fn tool_spec_is_send_sync() {
        assert_send_sync::<ToolSpec>();
    }
}
