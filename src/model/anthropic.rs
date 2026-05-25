//! Anthropic ARModel implementation.

use std::collections::HashMap;
use std::sync::Arc;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use super::{ARModel, ContentPart, Error, ModelInput, ModelStreamEvent, Role, Usage};
use crate::provider::{ModelInstanceConfig, ModelProvider, ProviderError};

// Hard-coded request-shape default; configurability (e.g. via ModelInstanceConfig extras) is deferred.
const MAX_TOKENS: u32 = 1024;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicModel {
    client: reqwest::Client,
    api_key: String,
    model_name: String,
    base_url: String,
}

/// `ModelProvider` implementation for Anthropic. Carries no per-provider
/// state today; reads `api_key` and an optional `base_url` from
/// `ModelInstanceConfig::extras` on each `build_model` call. The
/// `base_url` knob is the only Anthropic-specific dialect and is kept out
/// of the provider-agnostic `ModelProvider` trait by design.
#[derive(Default)]
pub struct AnthropicProvider;

impl AnthropicProvider {
    pub fn new() -> Self {
        Self
    }
}

impl ModelProvider for AnthropicProvider {
    fn build_model(&self, config: ModelInstanceConfig) -> Result<Arc<dyn ARModel>, ProviderError> {
        let extras = config
            .extras
            .as_object()
            .ok_or_else(|| ProviderError::BuildFailure {
                reason: "anthropic provider requires extras to be a JSON object".to_string(),
            })?;

        let api_key = match extras.get("api_key") {
            Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
            Some(_) => {
                return Err(ProviderError::BuildFailure {
                    reason:
                        "anthropic provider requires api_key in extras to be a non-empty string"
                            .to_string(),
                });
            }
            None => {
                return Err(ProviderError::BuildFailure {
                    reason: "anthropic provider requires api_key in extras".to_string(),
                });
            }
        };

        let base_url = match extras.get("base_url") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(_) => {
                return Err(ProviderError::BuildFailure {
                    reason: "anthropic provider requires base_url in extras to be a string"
                        .to_string(),
                });
            }
            None => DEFAULT_BASE_URL.to_string(),
        };

        Ok(Arc::new(AnthropicModel {
            client: reqwest::Client::new(),
            api_key,
            model_name: config.model_name,
            base_url,
        }))
    }
}

#[derive(serde::Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    system: &'a str,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
}

#[derive(serde::Serialize)]
struct AnthropicMessage {
    role: &'static str,
    content: Vec<AnthropicContentBlock>,
}

#[derive(serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: AnthropicToolResultContent,
        #[serde(skip_serializing_if = "is_false")]
        is_error: bool,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Wire shape for `tool_result.content`. Anthropic rejects raw JSON objects
/// here; the newtype constrains the field so a `serde_json::Value` cannot be
/// assigned to it accidentally. Serializes transparently as a bare string.
#[derive(serde::Serialize)]
#[serde(transparent)]
struct AnthropicToolResultContent(String);

/// The single sanctioned path from the portable `serde_json::Value` carried by
/// `ContentPart::ToolResult` into the constrained Anthropic wire shape.
/// String values pass through; every other JSON shape is rendered as a JSON
/// string so the model receives a faithful, lossless representation.
fn tool_result_content_from_value(v: &serde_json::Value) -> AnthropicToolResultContent {
    match v {
        serde_json::Value::String(s) => AnthropicToolResultContent(s.clone()),
        other => AnthropicToolResultContent(other.to_string()),
    }
}

#[derive(serde::Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

fn model_input_to_anthropic(
    input: &ModelInput,
) -> (String, Vec<AnthropicMessage>, Vec<AnthropicTool>) {
    let mut system = String::new();
    let mut messages = Vec::with_capacity(input.turns.len());
    for turn in &input.turns {
        match turn.role {
            Role::System => {
                for part in &turn.content {
                    match part {
                        ContentPart::Text(text) => {
                            if !system.is_empty() {
                                system.push_str("\n\n");
                            }
                            system.push_str(text);
                        }
                        ContentPart::ToolUse { .. } | ContentPart::ToolResult { .. } => {
                            // Tool exchanges have no meaningful place in a system turn; drop them.
                        }
                    }
                }
            }
            Role::User | Role::Assistant => {
                let role: &'static str = match turn.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::System => unreachable!(),
                };
                let mut content = Vec::with_capacity(turn.content.len());
                for part in &turn.content {
                    match part {
                        ContentPart::Text(t) => {
                            content.push(AnthropicContentBlock::Text { text: t.clone() })
                        }
                        ContentPart::ToolUse { id, name, input } => {
                            content.push(AnthropicContentBlock::ToolUse {
                                id: id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                            });
                        }
                        ContentPart::ToolResult {
                            tool_use_id,
                            content: result_content,
                            is_error,
                        } => {
                            content.push(AnthropicContentBlock::ToolResult {
                                tool_use_id: tool_use_id.clone(),
                                content: tool_result_content_from_value(result_content),
                                is_error: *is_error,
                            });
                        }
                    }
                }
                messages.push(AnthropicMessage { role, content });
            }
        }
    }
    let tools = input
        .tools
        .iter()
        .map(|spec| AnthropicTool {
            name: spec.name.clone(),
            description: spec.description.clone(),
            input_schema: spec.input_schema.clone(),
        })
        .collect();
    (system, messages, tools)
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

#[derive(serde::Deserialize)]
struct MessageStartPayload {
    message: MessageStartInner,
}

#[derive(serde::Deserialize)]
struct MessageStartInner {
    id: String,
    model: String,
    usage: MessageStartUsage,
}

#[derive(serde::Deserialize)]
struct MessageStartUsage {
    input_tokens: u32,
}

#[derive(serde::Deserialize)]
struct ContentBlockStartPayload {
    index: u32,
    content_block: ContentBlockStartInner,
}

#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockStartInner {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        // Anthropic always sends an empty object at start; the meaningful value
        // arrives via input_json_delta. The field is parsed for completeness.
        #[serde(default)]
        #[allow(dead_code)]
        input: serde_json::Value,
    },
    #[serde(other)]
    Other,
}

#[derive(serde::Deserialize)]
struct ContentBlockDeltaPayload {
    index: u32,
    delta: ContentDelta,
}

#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum ContentDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(other)]
    Other,
}

#[derive(serde::Deserialize)]
struct ContentBlockStopPayload {
    index: u32,
}

enum BlockState {
    Text {
        accumulator: String,
    },
    ToolUse {
        id: String,
        name: String,
        json_accumulator: String,
    },
}

#[derive(serde::Deserialize)]
struct MessageDeltaPayload {
    usage: MessageDeltaUsage,
}

#[derive(serde::Deserialize)]
struct MessageDeltaUsage {
    output_tokens: u32,
}

impl ARModel for AnthropicModel {
    fn complete<'a>(
        &'a self,
        input: &'a ModelInput,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<ModelStreamEvent, Error>> + Send + 'a>>
    {
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let model_name = self.model_name.clone();
        let base_url = self.base_url.clone();
        let (system, messages, tools) = model_input_to_anthropic(input);

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ModelStreamEvent, Error>>(64);

        tokio::spawn(async move {
            let request_body = AnthropicRequest {
                model: &model_name,
                max_tokens: MAX_TOKENS,
                stream: true,
                system: &system,
                messages,
                tools,
            };
            let url = format!("{base_url}/v1/messages");
            let response = match client
                .post(&url)
                .header("x-api-key", &api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .json(&request_body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(Error::from(e))).await;
                    return;
                }
            };

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                let _ = tx
                    .send(Err(Error::Api {
                        status,
                        message: body,
                    }))
                    .await;
                return;
            }

            let byte_stream = response.bytes_stream();
            let mut events = byte_stream.eventsource();
            let mut blocks: HashMap<u32, BlockState> = HashMap::new();
            let mut last_input_tokens: u32 = 0;
            let mut last_output_tokens: u32 = 0;

            while let Some(event) = events.next().await {
                let event = match event {
                    Ok(ev) => ev,
                    Err(e) => {
                        let _ = tx.send(Err(Error::Deserialization(e.to_string()))).await;
                        return;
                    }
                };

                match event.event.as_str() {
                    "message_start" => {
                        let payload: MessageStartPayload = match serde_json::from_str(&event.data) {
                            Ok(p) => p,
                            Err(e) => {
                                let _ = tx.send(Err(Error::Deserialization(e.to_string()))).await;
                                return;
                            }
                        };
                        last_input_tokens = payload.message.usage.input_tokens;
                        if tx
                            .send(Ok(ModelStreamEvent::MessageStart {
                                message_id: payload.message.id,
                                model: payload.message.model,
                            }))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        if tx
                            .send(Ok(ModelStreamEvent::Usage(Usage {
                                input_tokens: last_input_tokens,
                                output_tokens: 0,
                            })))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    "content_block_start" => {
                        let payload: ContentBlockStartPayload =
                            match serde_json::from_str(&event.data) {
                                Ok(p) => p,
                                Err(e) => {
                                    let _ =
                                        tx.send(Err(Error::Deserialization(e.to_string()))).await;
                                    return;
                                }
                            };
                        match payload.content_block {
                            ContentBlockStartInner::Text { text } => {
                                blocks
                                    .insert(payload.index, BlockState::Text { accumulator: text });
                            }
                            ContentBlockStartInner::ToolUse { id, name, .. } => {
                                blocks.insert(
                                    payload.index,
                                    BlockState::ToolUse {
                                        id: id.clone(),
                                        name: name.clone(),
                                        json_accumulator: String::new(),
                                    },
                                );
                                if tx
                                    .send(Ok(ModelStreamEvent::ToolUseStart { id, name }))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            ContentBlockStartInner::Other => {}
                        }
                    }
                    "content_block_delta" => {
                        let payload: ContentBlockDeltaPayload =
                            match serde_json::from_str(&event.data) {
                                Ok(p) => p,
                                Err(e) => {
                                    let _ =
                                        tx.send(Err(Error::Deserialization(e.to_string()))).await;
                                    return;
                                }
                            };
                        match (blocks.get_mut(&payload.index), payload.delta) {
                            (
                                Some(BlockState::Text { accumulator }),
                                ContentDelta::TextDelta { text },
                            ) => {
                                accumulator.push_str(&text);
                                if tx
                                    .send(Ok(ModelStreamEvent::ContentDelta { text }))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            (
                                Some(BlockState::ToolUse {
                                    id,
                                    json_accumulator,
                                    ..
                                }),
                                ContentDelta::InputJsonDelta { partial_json },
                            ) => {
                                json_accumulator.push_str(&partial_json);
                                if tx
                                    .send(Ok(ModelStreamEvent::ToolUseDelta {
                                        id: id.clone(),
                                        partial_json,
                                    }))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            _ => {}
                        }
                    }
                    "content_block_stop" => {
                        let payload: ContentBlockStopPayload =
                            match serde_json::from_str(&event.data) {
                                Ok(p) => p,
                                Err(e) => {
                                    let _ =
                                        tx.send(Err(Error::Deserialization(e.to_string()))).await;
                                    return;
                                }
                            };
                        let completed = match blocks.remove(&payload.index) {
                            Some(BlockState::Text { accumulator }) => {
                                Some(ModelStreamEvent::ContentComplete { text: accumulator })
                            }
                            Some(BlockState::ToolUse {
                                id,
                                name,
                                json_accumulator,
                            }) => {
                                match serde_json::from_str::<serde_json::Value>(&json_accumulator) {
                                    Ok(input) => {
                                        Some(ModelStreamEvent::ToolUseComplete { id, name, input })
                                    }
                                    Err(e) => {
                                        let _ = tx
                                            .send(Err(Error::Deserialization(format!(
                                                "tool_use input parse: {e}"
                                            ))))
                                            .await;
                                        return;
                                    }
                                }
                            }
                            None => None,
                        };
                        if let Some(event) = completed
                            && tx.send(Ok(event)).await.is_err()
                        {
                            return;
                        }
                    }
                    "message_delta" => {
                        let payload: MessageDeltaPayload = match serde_json::from_str(&event.data) {
                            Ok(p) => p,
                            Err(e) => {
                                let _ = tx.send(Err(Error::Deserialization(e.to_string()))).await;
                                return;
                            }
                        };
                        last_output_tokens = payload.usage.output_tokens;
                        if tx
                            .send(Ok(ModelStreamEvent::Usage(Usage {
                                input_tokens: last_input_tokens,
                                output_tokens: last_output_tokens,
                            })))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    "message_stop" => {
                        let _ = tx
                            .send(Ok(ModelStreamEvent::MessageComplete {
                                usage: Usage {
                                    input_tokens: last_input_tokens,
                                    output_tokens: last_output_tokens,
                                },
                            }))
                            .await;
                        return;
                    }
                    _ => {}
                }
            }
        });

        Box::pin(ReceiverStream::new(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ToolSpec, Turn};

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn provider_build_requires_api_key() {
        let provider = AnthropicProvider::new();
        let result = provider.build_model(ModelInstanceConfig::new("x", serde_json::json!({})));
        match result {
            Err(ProviderError::BuildFailure { reason }) => {
                assert!(
                    reason.contains("api_key"),
                    "expected api_key in reason, got {reason:?}"
                );
            }
            Err(e) => panic!("expected BuildFailure, got {e:?}"),
            Ok(_) => panic!("expected BuildFailure, got Ok"),
        }
    }

    #[test]
    fn provider_build_rejects_non_string_api_key() {
        let provider = AnthropicProvider::new();
        let result = provider.build_model(ModelInstanceConfig::new(
            "x",
            serde_json::json!({ "api_key": 42 }),
        ));
        match result {
            Err(ProviderError::BuildFailure { reason }) => {
                assert!(
                    reason.contains("api_key"),
                    "expected api_key in reason, got {reason:?}"
                );
            }
            Err(e) => panic!("expected BuildFailure, got {e:?}"),
            Ok(_) => panic!("expected BuildFailure, got Ok"),
        }
    }

    #[test]
    fn provider_build_rejects_non_object_extras() {
        let provider = AnthropicProvider::new();
        let result = provider.build_model(ModelInstanceConfig::new("x", serde_json::Value::Null));
        match result {
            Err(ProviderError::BuildFailure { reason }) => {
                assert!(
                    reason.contains("JSON object"),
                    "expected JSON object in reason, got {reason:?}"
                );
            }
            Err(e) => panic!("expected BuildFailure, got {e:?}"),
            Ok(_) => panic!("expected BuildFailure, got Ok"),
        }
    }

    #[test]
    fn provider_build_succeeds_with_minimum_extras() {
        let provider = AnthropicProvider::new();
        provider
            .build_model(ModelInstanceConfig::new(
                "claude-test",
                serde_json::json!({ "api_key": "k" }),
            ))
            .expect("provider builds with api_key alone");
    }

    #[test]
    fn provider_build_rejects_non_string_base_url() {
        let provider = AnthropicProvider::new();
        let result = provider.build_model(ModelInstanceConfig::new(
            "x",
            serde_json::json!({ "api_key": "k", "base_url": 42 }),
        ));
        match result {
            Err(ProviderError::BuildFailure { reason }) => {
                assert!(
                    reason.contains("base_url"),
                    "expected base_url in reason, got {reason:?}"
                );
            }
            Err(e) => panic!("expected BuildFailure, got {e:?}"),
            Ok(_) => panic!("expected BuildFailure, got Ok"),
        }
    }

    #[test]
    fn anthropic_model_is_send_sync() {
        assert_send_sync::<AnthropicModel>();
    }

    #[test]
    fn anthropic_provider_is_send_sync() {
        assert_send_sync::<AnthropicProvider>();
    }

    #[test]
    fn serializes_user_messages_correctly() {
        let input = ModelInput {
            turns: vec![
                Turn {
                    role: Role::System,
                    content: vec![ContentPart::Text("sys".to_string())],
                },
                Turn {
                    role: Role::User,
                    content: vec![ContentPart::Text("hello".to_string())],
                },
            ],
            tools: Vec::new(),
        };
        let (system, messages, tools) = model_input_to_anthropic(&input);
        assert_eq!(system, "sys");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content.len(), 1);
        assert!(tools.is_empty());

        let request = AnthropicRequest {
            model: "test-model",
            max_tokens: MAX_TOKENS,
            stream: true,
            system: &system,
            messages,
            tools,
        };
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["model"], "test-model");
        assert_eq!(value["max_tokens"], 1024);
        assert_eq!(value["stream"], true);
        assert_eq!(value["system"], "sys");
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(value["messages"][0]["content"][0]["type"], "text");
        assert_eq!(value["messages"][0]["content"][0]["text"], "hello");
        assert!(value.get("tools").is_none());
    }

    #[test]
    fn system_turns_concatenated_into_system_field() {
        let input = ModelInput {
            turns: vec![
                Turn {
                    role: Role::System,
                    content: vec![ContentPart::Text("first".to_string())],
                },
                Turn {
                    role: Role::System,
                    content: vec![ContentPart::Text("second".to_string())],
                },
                Turn {
                    role: Role::User,
                    content: vec![ContentPart::Text("hi".to_string())],
                },
            ],
            tools: Vec::new(),
        };
        let (system, messages, tools) = model_input_to_anthropic(&input);
        assert_eq!(system, "first\n\nsecond");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content.len(), 1);
        assert!(tools.is_empty());

        let request = AnthropicRequest {
            model: "test-model",
            max_tokens: MAX_TOKENS,
            stream: true,
            system: &system,
            messages,
            tools,
        };
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["messages"][0]["content"][0]["type"], "text");
        assert_eq!(value["messages"][0]["content"][0]["text"], "hi");
        assert!(value.get("tools").is_none());
    }

    #[test]
    fn serializes_tools_field_when_present() {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::User,
                content: vec![ContentPart::Text("hi".into())],
            }],
            tools: vec![ToolSpec {
                name: "echo".into(),
                description: "d".into(),
                input_schema: serde_json::json!({ "type": "object" }),
            }],
        };
        let (system, messages, tools) = model_input_to_anthropic(&input);
        let request = AnthropicRequest {
            model: "test-model",
            max_tokens: MAX_TOKENS,
            stream: true,
            system: &system,
            messages,
            tools,
        };
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["tools"][0]["name"], "echo");
        assert_eq!(value["tools"][0]["description"], "d");
        assert_eq!(value["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn serializes_tool_use_content_block() {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse {
                    id: "use_1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({ "text": "hi" }),
                }],
            }],
            tools: Vec::new(),
        };
        let (system, messages, tools) = model_input_to_anthropic(&input);
        let request = AnthropicRequest {
            model: "test-model",
            max_tokens: MAX_TOKENS,
            stream: true,
            system: &system,
            messages,
            tools,
        };
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["messages"][0]["role"], "assistant");
        assert_eq!(value["messages"][0]["content"][0]["type"], "tool_use");
        assert_eq!(value["messages"][0]["content"][0]["id"], "use_1");
        assert_eq!(value["messages"][0]["content"][0]["name"], "echo");
        assert_eq!(value["messages"][0]["content"][0]["input"]["text"], "hi");
    }

    #[test]
    fn serializes_tool_result_content_block() {
        let input_ok = ModelInput {
            turns: vec![Turn {
                role: Role::User,
                content: vec![ContentPart::ToolResult {
                    tool_use_id: "use_1".into(),
                    content: serde_json::json!("ok"),
                    is_error: false,
                }],
            }],
            tools: Vec::new(),
        };
        let (system, messages, tools) = model_input_to_anthropic(&input_ok);
        let request = AnthropicRequest {
            model: "test-model",
            max_tokens: MAX_TOKENS,
            stream: true,
            system: &system,
            messages,
            tools,
        };
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["messages"][0]["content"][0]["type"], "tool_result");
        assert_eq!(value["messages"][0]["content"][0]["tool_use_id"], "use_1");
        assert_eq!(value["messages"][0]["content"][0]["content"], "ok");
        assert!(value["messages"][0]["content"][0].get("is_error").is_none());

        let input_err = ModelInput {
            turns: vec![Turn {
                role: Role::User,
                content: vec![ContentPart::ToolResult {
                    tool_use_id: "use_1".into(),
                    content: serde_json::json!("ok"),
                    is_error: true,
                }],
            }],
            tools: Vec::new(),
        };
        let (system, messages, tools) = model_input_to_anthropic(&input_err);
        let request = AnthropicRequest {
            model: "test-model",
            max_tokens: MAX_TOKENS,
            stream: true,
            system: &system,
            messages,
            tools,
        };
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["messages"][0]["content"][0]["is_error"], true);
    }

    #[test]
    fn tool_result_object_value_is_stringified_for_anthropic() {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::User,
                content: vec![ContentPart::ToolResult {
                    tool_use_id: "use_1".into(),
                    content: serde_json::json!({ "content": "# README" }),
                    is_error: false,
                }],
            }],
            tools: Vec::new(),
        };
        let (system, messages, tools) = model_input_to_anthropic(&input);
        let request = AnthropicRequest {
            model: "test-model",
            max_tokens: MAX_TOKENS,
            stream: true,
            system: &system,
            messages,
            tools,
        };
        let value = serde_json::to_value(&request).expect("serialize request");
        let content = &value["messages"][0]["content"][0]["content"];
        assert!(
            content.is_string(),
            "tool_result.content must be a string, got {content}"
        );
        assert_eq!(content.as_str().unwrap(), r##"{"content":"# README"}"##);
    }

    #[test]
    fn serializes_mixed_text_and_tool_use_blocks() {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::Assistant,
                content: vec![
                    ContentPart::Text("thinking...".into()),
                    ContentPart::ToolUse {
                        id: "use_2".into(),
                        name: "echo".into(),
                        input: serde_json::Value::Null,
                    },
                ],
            }],
            tools: Vec::new(),
        };
        let (system, messages, tools) = model_input_to_anthropic(&input);
        let request = AnthropicRequest {
            model: "test-model",
            max_tokens: MAX_TOKENS,
            stream: true,
            system: &system,
            messages,
            tools,
        };
        let value = serde_json::to_value(&request).expect("serialize request");
        let content = value["messages"][0]["content"]
            .as_array()
            .expect("content is array");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_use");
    }

    #[test]
    fn parses_content_block_start_text() {
        let data =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let payload: ContentBlockStartPayload =
            serde_json::from_str(data).expect("parse content_block_start text");
        assert_eq!(payload.index, 0);
        match payload.content_block {
            ContentBlockStartInner::Text { text } => assert_eq!(text, ""),
            _ => panic!("expected ContentBlockStartInner::Text"),
        }
    }

    #[test]
    fn parses_content_block_start_tool_use() {
        let data = r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01abc","name":"echo","input":{}}}"#;
        let payload: ContentBlockStartPayload =
            serde_json::from_str(data).expect("parse content_block_start tool_use");
        assert_eq!(payload.index, 1);
        match payload.content_block {
            ContentBlockStartInner::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_01abc");
                assert_eq!(name, "echo");
                assert_eq!(input, serde_json::json!({}));
            }
            _ => panic!("expected ContentBlockStartInner::ToolUse"),
        }
    }

    #[test]
    fn parses_content_block_start_unknown_type_falls_back_to_other() {
        let data = r#"{"type":"content_block_start","index":3,"content_block":{"type":"image"}}"#;
        let payload: ContentBlockStartPayload =
            serde_json::from_str(data).expect("parse content_block_start unknown");
        assert_eq!(payload.index, 3);
        match payload.content_block {
            ContentBlockStartInner::Other => {}
            _ => panic!("expected ContentBlockStartInner::Other"),
        }
    }

    #[test]
    fn parses_input_json_delta() {
        let data = r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"hello\":\"wo"}}"#;
        let payload: ContentBlockDeltaPayload =
            serde_json::from_str(data).expect("parse input_json_delta");
        assert_eq!(payload.index, 1);
        match payload.delta {
            ContentDelta::InputJsonDelta { partial_json } => {
                assert_eq!(partial_json, "{\"hello\":\"wo");
            }
            _ => panic!("expected ContentDelta::InputJsonDelta"),
        }
    }

    #[test]
    fn accumulated_tool_use_json_round_trips_via_serde_json() {
        let fragments = ["{\"text\"", ":\"hi\"", "}"];
        let mut joined = String::new();
        for fragment in fragments {
            joined.push_str(fragment);
        }
        let value: serde_json::Value =
            serde_json::from_str(&joined).expect("parse accumulated tool_use input");
        assert_eq!(value, serde_json::json!({"text":"hi"}));
    }
}
