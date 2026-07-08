//! OpenRouter ARModel implementation, speaking the OpenAI ChatCompletions dialect.

use std::sync::Arc;

use super::openai_dialect::{
    MAX_TOKENS, OpenAiRequest, StreamOptions, model_input_to_openai, stream_openai_chat,
};
use super::{ARModel, Error, ModelInput, ModelStreamEvent};
use crate::provider::{ModelInstanceConfig, ModelProvider, ProviderError};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api";

pub struct OpenRouterModel {
    client: reqwest::Client,
    api_key: String,
    model_name: String,
    base_url: String,
}

/// `ModelProvider` implementation for OpenRouter. Carries no per-provider state
/// today; reads `api_key` and an optional `base_url` from
/// `ModelInstanceConfig::extras` on each `build_model` call. The `base_url`
/// knob is the only OpenRouter-specific dialect kept out of the
/// provider-agnostic `ModelProvider` trait by design.
#[derive(Default)]
pub struct OpenRouterProvider;

impl OpenRouterProvider {
    pub fn new() -> Self {
        Self
    }
}

impl ModelProvider for OpenRouterProvider {
    fn build_model(&self, config: ModelInstanceConfig) -> Result<Arc<dyn ARModel>, ProviderError> {
        let (api_key, base_url) = crate::provider::parse_api_key_and_base_url(
            &config.extras,
            "openrouter",
            DEFAULT_BASE_URL,
        )?;

        Ok(Arc::new(OpenRouterModel {
            client: reqwest::Client::new(),
            api_key,
            model_name: config.model_name,
            base_url,
        }))
    }
}

impl ARModel for OpenRouterModel {
    fn complete<'a>(
        &'a self,
        input: &'a ModelInput,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<ModelStreamEvent, Error>> + Send + 'a>>
    {
        let (messages, tools) = model_input_to_openai(input);
        let request_body = OpenAiRequest {
            model: &self.model_name,
            messages,
            stream: true,
            max_tokens: MAX_TOKENS,
            stream_options: StreamOptions {
                include_usage: true,
            },
            tools,
        };
        let body = match serde_json::to_value(&request_body) {
            Ok(v) => v,
            Err(e) => {
                let err = Error::from(e);
                return Box::pin(futures::stream::once(async move { Err(err) }));
            }
        };
        // Optional OpenRouter HTTP-Referer / X-Title attribution headers are
        // omitted; configurability is deferred.
        let url = format!("{}/v1/chat/completions", self.base_url);
        stream_openai_chat(self.client.clone(), url, self.api_key.clone(), body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ContentPart, Role, ToolSpec, Turn};

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn provider_build_requires_api_key() -> Result<(), Box<dyn std::error::Error>> {
        let provider = OpenRouterProvider::new();
        let result = provider.build_model(ModelInstanceConfig::new("x", serde_json::json!({})));
        match result {
            Err(ProviderError::BuildFailure { reason }) => {
                assert!(
                    reason.contains("api_key"),
                    "expected api_key in reason, got {reason:?}"
                );
            }
            Err(e) => return Err(format!("expected BuildFailure, got {e:?}").into()),
            Ok(_) => return Err("expected BuildFailure, got Ok".into()),
        }
        Ok(())
    }

    #[test]
    fn provider_build_rejects_non_string_api_key() -> Result<(), Box<dyn std::error::Error>> {
        let provider = OpenRouterProvider::new();
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
            Err(e) => return Err(format!("expected BuildFailure, got {e:?}").into()),
            Ok(_) => return Err("expected BuildFailure, got Ok".into()),
        }
        Ok(())
    }

    #[test]
    fn provider_build_rejects_non_object_extras() -> Result<(), Box<dyn std::error::Error>> {
        let provider = OpenRouterProvider::new();
        let result = provider.build_model(ModelInstanceConfig::new("x", serde_json::Value::Null));
        match result {
            Err(ProviderError::BuildFailure { reason }) => {
                assert!(
                    reason.contains("JSON object"),
                    "expected JSON object in reason, got {reason:?}"
                );
            }
            Err(e) => return Err(format!("expected BuildFailure, got {e:?}").into()),
            Ok(_) => return Err("expected BuildFailure, got Ok".into()),
        }
        Ok(())
    }

    #[test]
    fn provider_build_succeeds_with_minimum_extras() {
        let provider = OpenRouterProvider::new();
        provider
            .build_model(ModelInstanceConfig::new(
                "anthropic/claude-3.5-sonnet",
                serde_json::json!({ "api_key": "k" }),
            ))
            .expect("provider builds with api_key alone");
    }

    #[test]
    fn provider_build_rejects_non_string_base_url() -> Result<(), Box<dyn std::error::Error>> {
        let provider = OpenRouterProvider::new();
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
            Err(e) => return Err(format!("expected BuildFailure, got {e:?}").into()),
            Ok(_) => return Err("expected BuildFailure, got Ok".into()),
        }
        Ok(())
    }

    #[test]
    fn default_base_url_yields_chat_completions_url_and_advertises_tools()
    -> Result<(), Box<dyn std::error::Error>> {
        let url = format!("{}/v1/chat/completions", DEFAULT_BASE_URL);
        assert_eq!(url, "https://openrouter.ai/api/v1/chat/completions");

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
        let (messages, tools) = model_input_to_openai(&input);
        let request = OpenAiRequest {
            model: "anthropic/claude-3.5-sonnet",
            messages,
            stream: true,
            max_tokens: MAX_TOKENS,
            stream_options: StreamOptions {
                include_usage: true,
            },
            tools,
        };
        let value = serde_json::to_value(&request)?;
        assert_eq!(value["model"], "anthropic/claude-3.5-sonnet");
        assert_eq!(value["tools"][0]["function"]["name"], "echo");
        Ok(())
    }

    #[test]
    fn openrouter_model_is_send_sync() {
        assert_send_sync::<OpenRouterModel>();
    }

    #[test]
    fn openrouter_provider_is_send_sync() {
        assert_send_sync::<OpenRouterProvider>();
    }
}
