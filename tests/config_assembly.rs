//! Integration test for Phase F3 of the configuration-file plan: load the
//! embedded `default.toml` topology together with a fixture
//! `pristine-auth.toml`, drive [`pristine::build_harness_from_config`], and
//! assert the assembled `Harness` carries the expected agent, model, tools,
//! and provider registrations. The harness is never started; no live API
//! calls are made.

use std::path::PathBuf;

use pristine::HarnessAssemblyError;
use pristine::build_harness_from_config;
use pristine::config::{HomeSource, LoadArgs, load_with};
use pristine::test_support::MapEnv;

struct MockHome(Option<PathBuf>);

impl MockHome {
    fn none() -> Self {
        Self(None)
    }
}

impl HomeSource for MockHome {
    fn home_dir(&self) -> Option<PathBuf> {
        self.0.clone()
    }
}

/// Auth fixture mirrors the `AuthConfig` shape. The `default` model alias
/// points at the `anthropic` provider with `claude-opus-4-7` and an
/// `{{ANTHROPIC_API_KEY}}` placeholder that the templating layer substitutes
/// out of the supplied `MapEnv`.
const FIXTURE_AUTH: &str = r#"
[providers.anthropic]
type = "anthropic"

[models.default]
provider = "anthropic"
model_name = "claude-opus-4-7"
api_key = "{{ANTHROPIC_API_KEY}}"
"#;

#[test]
fn default_topology_and_fixture_auth_produce_expected_harness() {
    let dir = tempfile::tempdir().expect("tempdir");
    let auth_path = dir.path().join("pristine-auth.toml");
    std::fs::write(&auth_path, FIXTURE_AUTH).expect("write fixture auth");

    // `config: None` selects the embedded `default.toml`. `auth: Some(...)`
    // points at the tempdir fixture so the test never touches `~` or the real
    // user environment. `MockHome::none()` is safe here because both the
    // topology (None) and auth (absolute path) bypass home-dir lookup.
    let home = MockHome::none();
    let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-test-fixture")]);
    let args = LoadArgs {
        config: None,
        auth: Some(&auth_path),
    };

    let config = load_with(args, &home, &env).expect("load_with succeeds for default + fixture");

    let (harness, agent_ids) = match build_harness_from_config(config) {
        Ok(value) => value,
        Err(HarnessAssemblyError::Config(errors)) => {
            panic!("expected Ok harness, got Config errors: {errors}")
        }
        Err(HarnessAssemblyError::Other(err)) => {
            panic!("expected Ok harness, got Other: {err}")
        }
    };

    assert_eq!(agent_ids.len(), 1, "default topology declares one agent");

    let mut tool_names: Vec<String> = harness
        .tools()
        .list()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    tool_names.sort();
    assert_eq!(
        tool_names,
        vec![
            "edit".to_string(),
            "exec_bash".to_string(),
            "insert".to_string(),
            "read".to_string(),
            "write".to_string(),
        ],
        "harness registers the five built-in tools",
    );

    assert!(
        harness.provider_registry().get("anthropic").is_some(),
        "harness provider registry contains `anthropic`",
    );
}

#[test]
fn loaded_config_carries_resolved_model_fields_for_default_agent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let auth_path = dir.path().join("pristine-auth.toml");
    std::fs::write(&auth_path, FIXTURE_AUTH).expect("write fixture auth");

    let home = MockHome::none();
    let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-test-fixture")]);
    let args = LoadArgs {
        config: None,
        auth: Some(&auth_path),
    };

    let config = load_with(args, &home, &env).expect("load_with succeeds");

    assert_eq!(config.agents.len(), 1);
    let agent = &config.agents[0];
    assert_eq!(agent.name, "default");
    assert_eq!(agent.model.alias, "default");
    assert_eq!(agent.model.provider_name, "anthropic");
    assert_eq!(agent.model.model_name, "claude-opus-4-7");
    assert_eq!(
        agent.model.api_key, "sk-test-fixture",
        "templating substitutes {{{{ANTHROPIC_API_KEY}}}} into the resolved api_key",
    );
}
