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
/// `base_url`) is decoded inline. v1 ships only `Anthropic`; future providers
/// plug in here.
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
}

/// One entry of `[models.X]` in the auth file. Points at a `[providers.Y]`
/// entry by name and carries the provider-native model name plus the api key
/// used when invoking that model. The api key is a verbatim string at this
/// stage; the templating layer (Phase B3) substitutes `{{ENV_VAR}}`
/// placeholders before this struct is constructed.
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
    fn provider_kind_anthropic_with_default_base_url() {
        let toml = r#"
type = "anthropic"
"#;
        let cfg: ProviderConfig = toml::from_str(toml).expect("anthropic provider deserializes");
        match cfg {
            ProviderConfig::Anthropic { base_url } => assert!(base_url.is_none()),
        }
    }
}
