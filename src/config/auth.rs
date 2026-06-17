//! Auth configuration — providers, model aliases, credentials.
//!
//! Mirrors the user-global `~/pristine-auth.toml`. Identity is decoupled from
//! topology: a single auth file may back many topology files. The api key
//! lives on each `ModelAliasConfig` rather than on its provider so that
//! different aliases pointing at the same provider can carry different
//! credentials.

use std::collections::HashMap;

use serde::Deserialize;

/// Root of the auth file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: HashMap<String, ModelAliasConfig>,
}

/// One entry of `[providers.X]` in the auth file. The `type` discriminator
/// selects the variant; provider-specific dialect (e.g. Anthropic's
/// `base_url`) is decoded inline. `rename_all = "snake_case"` means the TOML
/// discriminators are `type = "anthropic"`, `type = "deep_seek"`, and
/// `type = "open_router"`.
///
/// `ProviderConfig` is a directly-tagged enum because serde's `flatten` is
/// incompatible with `deny_unknown_fields`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderConfig {
    Anthropic {
        #[serde(default)]
        base_url: Option<String>,
    },
    DeepSeek {
        #[serde(default)]
        base_url: Option<String>,
    },
    OpenRouter {
        #[serde(default)]
        base_url: Option<String>,
    },
}

/// One entry of `[models.X]` in the auth file. Points at a `[providers.Y]`
/// entry by name and carries the provider-native model name plus the api key
/// used when invoking that model. The api key is a verbatim string at this
/// stage; `config::template` substitutes `{{ENV_VAR}}` placeholders against
/// the process environment before this struct is constructed.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAliasConfig {
    pub provider: String,
    pub model_name: String,
    pub api_key: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_config_rejects_unknown_top_level_field() {
        let toml = r#"
mystery = true
"#;
        let err = toml::from_str::<AuthConfig>(toml).expect_err("unknown field is rejected");
        assert!(
            err.to_string().contains("mystery"),
            "error mentions the unknown field: {err}"
        );
    }

    #[test]
    fn model_alias_rejects_unknown_field() {
        let toml = r#"
provider = "anthropic"
model_name = "claude-opus-4-7"
api_key = "xxx"
extra = "no"
"#;
        let err = toml::from_str::<ModelAliasConfig>(toml).expect_err("unknown field is rejected");
        assert!(
            err.to_string().contains("extra"),
            "error mentions the unknown field: {err}"
        );
    }

    #[test]
    fn provider_kind_anthropic_with_default_base_url() -> Result<(), Box<dyn std::error::Error>> {
        let toml = r#"
type = "anthropic"
"#;
        let cfg: ProviderConfig = toml::from_str(toml)?;
        match cfg {
            ProviderConfig::Anthropic { base_url } => assert!(base_url.is_none()),
            other => return Err(format!("expected Anthropic variant, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn provider_kind_deepseek_with_default_base_url() -> Result<(), Box<dyn std::error::Error>> {
        // The `deep_seek` discriminator is the snake_case rename of `DeepSeek`.
        let toml = r#"
type = "deep_seek"
"#;
        let cfg: ProviderConfig = toml::from_str(toml)?;
        match cfg {
            ProviderConfig::DeepSeek { base_url } => assert!(base_url.is_none()),
            other => return Err(format!("expected DeepSeek variant, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn provider_kind_openrouter_with_default_base_url() -> Result<(), Box<dyn std::error::Error>> {
        // The `open_router` discriminator is the snake_case rename of `OpenRouter`.
        let toml = r#"
type = "open_router"
"#;
        let cfg: ProviderConfig = toml::from_str(toml)?;
        match cfg {
            ProviderConfig::OpenRouter { base_url } => assert!(base_url.is_none()),
            other => return Err(format!("expected OpenRouter variant, got {other:?}").into()),
        }
        Ok(())
    }
}
