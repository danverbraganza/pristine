//! DeepSeek ARModel implementation, speaking the OpenAI ChatCompletions dialect.

use std::sync::Arc;

use super::openai_dialect::{
    MAX_TOKENS, OpenAiRequest, StreamOptions, model_input_to_openai, stream_openai_chat,
};
use super::{ARModel, Error, ModelInput, ModelStreamEvent};
use crate::provider::{ModelInstanceConfig, ModelProvider, ProviderError};

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";

pub struct DeepSeekModel {
    client: reqwest::Client,
    api_key: String,
    model_name: String,
    base_url: String,
}

/// `ModelProvider` implementation for DeepSeek. Carries no per-provider state
/// today; reads `api_key` and an optional `base_url` from
/// `ModelInstanceConfig::extras` on each `build_model` call. The `base_url`
/// knob is the only DeepSeek-specific dialect kept out of the
/// provider-agnostic `ModelProvider` trait by design.
#[derive(Default)]
pub struct DeepSeekProvider;

impl DeepSeekProvider {
    pub fn new() -> Self {
        Self
    }
}

impl ModelProvider for DeepSeekProvider {
    fn build_model(&self, config: ModelInstanceConfig) -> Result<Arc<dyn ARModel>, ProviderError> {
        let (api_key, base_url) = crate::provider::parse_api_key_and_base_url(
            &config.extras,
            "deepseek",
            DEFAULT_BASE_URL,
        )?;

        Ok(Arc::new(DeepSeekModel {
            client: reqwest::Client::new(),
            api_key,
            model_name: config.model_name,
            base_url,
        }))
    }
}

impl ARModel for DeepSeekModel {
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
        let url = format!("{}/v1/chat/completions", self.base_url);
        stream_openai_chat(self.client.clone(), url, self.api_key.clone(), body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn provider_build_requires_api_key() -> Result<(), Box<dyn std::error::Error>> {
        let provider = DeepSeekProvider::new();
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
        let provider = DeepSeekProvider::new();
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
        let provider = DeepSeekProvider::new();
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
        let provider = DeepSeekProvider::new();
        provider
            .build_model(ModelInstanceConfig::new(
                "deepseek-v4-pro",
                serde_json::json!({ "api_key": "k" }),
            ))
            .expect("provider builds with api_key alone");
    }

    #[test]
    fn provider_build_rejects_non_string_base_url() -> Result<(), Box<dyn std::error::Error>> {
        let provider = DeepSeekProvider::new();
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
    fn deepseek_model_is_send_sync() {
        assert_send_sync::<DeepSeekModel>();
    }

    #[test]
    fn deepseek_provider_is_send_sync() {
        assert_send_sync::<DeepSeekProvider>();
    }
}
