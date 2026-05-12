//! Anthropic ARModel implementation.

use eventsource_stream::Eventsource;
use futures::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use crate::history::Block;

use super::{ARModel, Error, ModelStreamEvent, Usage};

// Phase 1 cap; revisited when tool use / config land.
const MAX_TOKENS: u32 = 1024;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicModel {
    client: reqwest::Client,
    api_key: String,
    model_name: String,
    base_url: String,
}

pub struct AnthropicModelBuilder {
    api_key: Option<String>,
    model_name: Option<String>,
    base_url: Option<String>,
}

impl AnthropicModelBuilder {
    pub fn new() -> Self {
        Self {
            api_key: None,
            model_name: None,
            base_url: None,
        }
    }

    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    pub fn model_name(mut self, name: impl Into<String>) -> Self {
        self.model_name = Some(name.into());
        self
    }

    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    pub fn build(self) -> Result<AnthropicModel, Error> {
        let api_key = self
            .api_key
            .ok_or_else(|| Error::Configuration("missing api_key".to_string()))?;
        let model_name = self
            .model_name
            .ok_or_else(|| Error::Configuration("missing model_name".to_string()))?;
        let base_url = self
            .base_url
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Ok(AnthropicModel {
            client: reqwest::Client::new(),
            api_key,
            model_name,
            base_url,
        })
    }
}

impl Default for AnthropicModelBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(serde::Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    system: &'a str,
    messages: Vec<AnthropicMessage>,
}

#[derive(serde::Serialize)]
struct AnthropicMessage {
    role: &'static str,
    content: String,
}

fn block_to_message(block: &Block) -> Option<AnthropicMessage> {
    match block {
        Block::UserMessage { content, .. } => Some(AnthropicMessage {
            role: "user",
            content: content.clone(),
        }),
        Block::AgentMessage { content, .. } => Some(AnthropicMessage {
            role: "assistant",
            content: content.clone(),
        }),
        Block::ReasoningTrace { .. } | Block::ToolCall { .. } | Block::ToolResult { .. } => None,
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
struct ContentBlockDeltaPayload {
    delta: ContentDelta,
}

#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum ContentDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(other)]
    Other,
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
        system_prompt: &'a str,
        messages: &'a [Block],
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<ModelStreamEvent, Error>> + Send + 'a>>
    {
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let model_name = self.model_name.clone();
        let base_url = self.base_url.clone();
        let messages: Vec<AnthropicMessage> =
            messages.iter().filter_map(block_to_message).collect();
        let system = system_prompt.to_owned();

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ModelStreamEvent, Error>>(64);

        tokio::spawn(async move {
            let request_body = AnthropicRequest {
                model: &model_name,
                max_tokens: MAX_TOKENS,
                stream: true,
                system: &system,
                messages,
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
            let mut accumulator = String::new();
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
                        accumulator.clear();
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
                        if let ContentDelta::TextDelta { text } = payload.delta {
                            accumulator.push_str(&text);
                            if tx
                                .send(Ok(ModelStreamEvent::ContentDelta { text }))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    "content_block_stop" => {
                        let text = std::mem::take(&mut accumulator);
                        if tx
                            .send(Ok(ModelStreamEvent::ContentComplete { text }))
                            .await
                            .is_err()
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
    use crate::history::UserId;
    use std::time::SystemTime;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn builder_requires_api_key() {
        let result = AnthropicModelBuilder::new().model_name("x").build();
        match result {
            Err(Error::Configuration(msg)) => assert_eq!(msg, "missing api_key"),
            Err(e) => panic!("expected Configuration error, got {e:?}"),
            Ok(_) => panic!("expected Configuration error, got Ok"),
        }
    }

    #[test]
    fn builder_requires_model_name() {
        let result = AnthropicModelBuilder::new().api_key("k").build();
        match result {
            Err(Error::Configuration(msg)) => assert_eq!(msg, "missing model_name"),
            Err(e) => panic!("expected Configuration error, got {e:?}"),
            Ok(_) => panic!("expected Configuration error, got Ok"),
        }
    }

    #[test]
    fn anthropic_model_is_send_sync() {
        assert_send_sync::<AnthropicModel>();
    }

    #[test]
    fn serializes_user_messages_correctly() {
        let blocks = [Block::UserMessage {
            from: UserId::new(),
            content: "hello".to_string(),
            timestamp: SystemTime::now(),
        }];
        let messages: Vec<AnthropicMessage> = blocks.iter().filter_map(block_to_message).collect();
        let request = AnthropicRequest {
            model: "test-model",
            max_tokens: MAX_TOKENS,
            stream: true,
            system: "test-system",
            messages,
        };
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["model"], "test-model");
        assert_eq!(value["max_tokens"], 1024);
        assert_eq!(value["stream"], true);
        assert_eq!(value["system"], "test-system");
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(value["messages"][0]["content"], "hello");
    }

    #[test]
    fn non_message_blocks_are_filtered_out() {
        let now = SystemTime::now();
        let blocks = [
            Block::ReasoningTrace {
                content: "thinking".to_string(),
                timestamp: now,
            },
            Block::ToolCall {
                name: "tool".to_string(),
                arguments: serde_json::json!({}),
                timestamp: now,
            },
            Block::ToolResult {
                name: "tool".to_string(),
                result: serde_json::json!({}),
                timestamp: now,
            },
            Block::UserMessage {
                from: UserId::new(),
                content: "hi".to_string(),
                timestamp: now,
            },
        ];
        let produced: Vec<AnthropicMessage> = blocks.iter().filter_map(block_to_message).collect();
        assert_eq!(produced.len(), 1);
        assert_eq!(produced[0].role, "user");
        assert_eq!(produced[0].content, "hi");
    }
}
