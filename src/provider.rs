//! `ModelProvider` trait, error type, and registry for constructing models
//! from configuration.
//!
//! Parallels `crate::tool` (trait + error enum + registry, all in one file).
//! A `ModelProvider` builds an `Arc<dyn ARModel>` from a `ModelInstanceConfig`
//! — a typed model name plus an opaque `serde_json::Value` carrier for
//! provider-specific extras (e.g. an Anthropic provider may consume
//! `api_key` and `base_url` from that carrier). The carrier keeps the trait
//! provider-agnostic: the registry and trait never name `AnthropicModel` or
//! any other concrete model type.

use std::collections::HashMap;
use std::sync::Arc;

use crate::model::ARModel;

/// Configuration passed to `ModelProvider::build_model`. `model_name` is the
/// provider-native model identifier (e.g. `"claude-sonnet-4-6"`); `extras`
/// carries provider-specific fields as opaque JSON so the trait does not
/// learn each provider's dialect.
#[derive(Clone, Debug)]
pub struct ModelInstanceConfig {
    pub model_name: String,
    pub extras: serde_json::Value,
}

impl ModelInstanceConfig {
    pub fn new(model_name: impl Into<String>, extras: serde_json::Value) -> Self {
        Self {
            model_name: model_name.into(),
            extras,
        }
    }
}

#[derive(Debug)]
pub enum ProviderError {
    BuildFailure { reason: String },
    DuplicateProvider(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::BuildFailure { reason } => {
                write!(f, "model provider build failure: {reason}")
            }
            ProviderError::DuplicateProvider(name) => {
                write!(f, "provider already registered: {name}")
            }
        }
    }
}

impl std::error::Error for ProviderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

/// Extracts the `api_key` and optional `base_url` credentials a provider reads
/// from `ModelInstanceConfig::extras`. `provider` names the caller for error
/// messages; `default_base_url` is used when `base_url` is absent. `extras`
/// must be a JSON object with a non-empty string `api_key` and an optional
/// string `base_url`.
pub(crate) fn parse_api_key_and_base_url(
    extras: &serde_json::Value,
    provider: &str,
    default_base_url: &str,
) -> Result<(String, String), ProviderError> {
    let extras = extras
        .as_object()
        .ok_or_else(|| ProviderError::BuildFailure {
            reason: format!("{provider} provider requires extras to be a JSON object"),
        })?;

    let api_key = match extras.get("api_key") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
        Some(_) => {
            return Err(ProviderError::BuildFailure {
                reason: format!(
                    "{provider} provider requires api_key in extras to be a non-empty string"
                ),
            });
        }
        None => {
            return Err(ProviderError::BuildFailure {
                reason: format!("{provider} provider requires api_key in extras"),
            });
        }
    };

    let base_url = match extras.get("base_url") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(_) => {
            return Err(ProviderError::BuildFailure {
                reason: format!("{provider} provider requires base_url in extras to be a string"),
            });
        }
        None => default_base_url.to_string(),
    };

    Ok((api_key, base_url))
}

/// Builds concrete `ARModel` instances from `ModelInstanceConfig`.
/// Provider impls own their own dialect — they pull whatever credentials and
/// options they need out of `config.extras` and surface any errors as
/// `ProviderError::BuildFailure`.
pub trait ModelProvider: Send + Sync {
    fn build_model(&self, config: ModelInstanceConfig) -> Result<Arc<dyn ARModel>, ProviderError>;
}

/// Owns the set of `ModelProvider`s available to the configuration loader.
/// Names are unique; attempts to register a duplicate are rejected rather than
/// silently overwriting an existing entry.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn ModelProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(
        &mut self,
        name: impl Into<String>,
        provider: Arc<dyn ModelProvider>,
    ) -> Result<(), ProviderError> {
        let name = name.into();
        if self.providers.contains_key(&name) {
            return Err(ProviderError::DuplicateProvider(name));
        }
        self.providers.insert(name, provider);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ModelProvider>> {
        self.providers.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::StubArModel;

    struct StubProvider;

    impl ModelProvider for StubProvider {
        fn build_model(
            &self,
            _config: ModelInstanceConfig,
        ) -> Result<Arc<dyn ARModel>, ProviderError> {
            Ok(Arc::new(StubArModel::empty()))
        }
    }

    fn assert_std_error<T: std::error::Error + Send + Sync + 'static>() {}

    #[test]
    fn provider_error_is_standard_error_trait_object() {
        assert_std_error::<ProviderError>();
    }

    #[test]
    fn add_then_get_round_trips_registered_provider() {
        let mut registry = ProviderRegistry::new();
        registry
            .add("stub", Arc::new(StubProvider))
            .expect("first registration succeeds");
        let provider = registry.get("stub").expect("registered provider visible");
        let model = provider
            .build_model(ModelInstanceConfig::new("model-x", serde_json::Value::Null))
            .expect("stub build succeeds");
        // Smoke check the trait object is usable — `complete` returns a stream
        // we do not need to drive here; constructing it is enough.
        let input = crate::model::ModelInput {
            turns: Vec::new(),
            tools: Vec::new(),
        };
        let _stream = model.complete(&input);
    }

    #[test]
    fn duplicate_add_returns_duplicate_provider_error() -> Result<(), Box<dyn std::error::Error>> {
        let mut registry = ProviderRegistry::new();
        registry
            .add("stub", Arc::new(StubProvider))
            .expect("first registration succeeds");
        let err = registry
            .add("stub", Arc::new(StubProvider))
            .expect_err("second registration fails");
        match err {
            ProviderError::DuplicateProvider(name) => assert_eq!(name, "stub"),
            other => return Err(format!("expected DuplicateProvider, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn get_missing_name_returns_none() {
        let registry = ProviderRegistry::new();
        assert!(registry.get("absent").is_none());
    }
}
