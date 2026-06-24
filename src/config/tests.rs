use super::*;
use crate::test_support::{MapEnv, MockHome};

const TOPOLOGY_PATH: &str = "/virtual/topology.toml";
const AUTH_PATH: &str = "/virtual/auth.toml";

fn valid_topology() -> &'static str {
    r#"
[[agents]]
name = "default"
model = "default"
system_prompt = "you are pristine"
tools = ["read", "write"]

[tools.read]
type = "builtin"
builtin = "read"

[tools.write]
type = "builtin"
builtin = "write"
"#
}

fn valid_auth() -> &'static str {
    r#"
[providers.anthropic]
type = "anthropic"

[models.default]
provider = "anthropic"
model_name = "claude-sonnet-4-6"
api_key = "{{ANTHROPIC_API_KEY}}"
"#
}

#[test]
fn assemble_config_happy_path() {
    let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-foo")]);
    let config = assemble_config(
        valid_topology(),
        Path::new(TOPOLOGY_PATH),
        valid_auth(),
        Path::new(AUTH_PATH),
        &env,
        None,
    )
    .expect("happy path assembles");

    assert_eq!(config.agents.len(), 1);
    let agent = &config.agents[0];
    assert_eq!(agent.name, "default");
    assert_eq!(agent.system_prompt, "you are pristine");
    assert_eq!(agent.tools, vec!["read".to_string(), "write".to_string()]);
    assert_eq!(agent.model.alias, "default");
    assert_eq!(agent.model.provider_name, "anthropic");
    assert_eq!(agent.model.model_name, "claude-sonnet-4-6");
    assert_eq!(agent.model.api_key, "sk-foo");
    assert_eq!(config.tools.len(), 2);
    assert!(config.tools.contains_key("read"));
    assert!(config.tools.contains_key("write"));
    assert_eq!(config.providers.len(), 1);
    assert!(config.providers.contains_key("anthropic"));
}

#[test]
fn assemble_config_topology_parse_error_collects_auth_errors_too() {
    let broken_topology = "this is not = = valid toml [[";
    let env = MapEnv::default();
    let errors = assemble_config(
        broken_topology,
        Path::new(TOPOLOGY_PATH),
        valid_auth(),
        Path::new(AUTH_PATH),
        &env,
        None,
    )
    .expect_err("broken topology + missing env should error");

    let mut saw_toml_parse_on_topology = false;
    let mut saw_unknown_env_on_auth = false;
    for err in errors.as_slice() {
        match err {
            ConfigError::TomlParse { path, .. } if path == Path::new(TOPOLOGY_PATH) => {
                saw_toml_parse_on_topology = true;
            }
            ConfigError::UnknownEnvVar { name, .. } if name == "ANTHROPIC_API_KEY" => {
                saw_unknown_env_on_auth = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_toml_parse_on_topology,
        "expected a TomlParse error for the topology file; got {errors}"
    );
    assert!(
        saw_unknown_env_on_auth,
        "expected an UnknownEnvVar error for ANTHROPIC_API_KEY; got {errors}"
    );
}

#[test]
fn assemble_config_dangling_alias_and_undeclared_tool_collected_together() {
    let topology = r#"
[[agents]]
name = "default"
model = "nope"
system_prompt = "hi"
tools = ["nonsense"]
"#;
    let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-foo")]);
    let errors = assemble_config(
        topology,
        Path::new(TOPOLOGY_PATH),
        valid_auth(),
        Path::new(AUTH_PATH),
        &env,
        None,
    )
    .expect_err("dangling alias + undeclared tool should error");

    let mut saw_dangling = false;
    let mut saw_undeclared = false;
    for err in errors.as_slice() {
        match err {
            ConfigError::DanglingAlias { alias } if alias == "nope" => {
                saw_dangling = true;
            }
            ConfigError::UndeclaredTool { agent, tool }
                if agent == "default" && tool == "nonsense" =>
            {
                saw_undeclared = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_dangling,
        "expected DanglingAlias for 'nope'; got {errors}"
    );
    assert!(
        saw_undeclared,
        "expected UndeclaredTool for agent 'default', tool 'nonsense'; got {errors}"
    );
}

#[test]
fn assemble_config_all_pass_returns_config() {
    let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-bar")]);
    let config = assemble_config(
        valid_topology(),
        Path::new(TOPOLOGY_PATH),
        valid_auth(),
        Path::new(AUTH_PATH),
        &env,
        None,
    )
    .expect("all-pass produces a Config");

    assert_eq!(config.agents.len(), 1);
    assert_eq!(config.tools.len(), 2);
    assert_eq!(config.providers.len(), 1);
    assert_eq!(config.agents[0].model.api_key, "sk-bar");
}

#[test]
fn load_with_succeeds_for_valid_paths_and_env() {
    let dir = tempfile::tempdir().expect("tempdir");
    let topology_path = dir.path().join("topology.toml");
    let auth_path = dir.path().join("auth.toml");
    std::fs::write(&topology_path, valid_topology()).expect("write topology");
    std::fs::write(&auth_path, valid_auth()).expect("write auth");

    let home = MockHome::some(dir.path().to_path_buf());
    let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-load")]);
    let args = LoadArgs {
        config: Some(&topology_path),
        auth: Some(&auth_path),
        model: None,
    };
    let config = load_with(args, &home, &env).expect("load_with succeeds");

    assert_eq!(config.agents.len(), 1);
    assert_eq!(config.agents[0].name, "default");
    assert_eq!(config.agents[0].model.api_key, "sk-load");
    assert_eq!(config.tools.len(), 2);
    assert_eq!(config.providers.len(), 1);
}

#[test]
fn load_with_embedded_default_topology_when_no_override() {
    let dir = tempfile::tempdir().expect("tempdir");
    let auth_path = dir.path().join("auth.toml");
    std::fs::write(&auth_path, valid_auth()).expect("write auth");

    let home = MockHome::some(dir.path().to_path_buf());
    let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-embedded")]);
    let args = LoadArgs {
        config: None,
        auth: Some(&auth_path),
        model: None,
    };
    let config = load_with(args, &home, &env).expect("embedded default loads");

    assert_eq!(config.agents.len(), 1);
    assert_eq!(config.agents[0].name, "default");
    assert_eq!(config.agents[0].model.alias, "default");
}

#[test]
fn load_with_auto_writes_missing_auth_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let topology_path = dir.path().join("topology.toml");
    std::fs::write(&topology_path, valid_topology()).expect("write topology");
    // Auth path under a nested subdirectory that does not exist; the
    // auto-write step must create it.
    let auth_path = dir.path().join("nested").join("pristine-auth.toml");
    assert!(!auth_path.exists(), "precondition: auth file absent");

    let home = MockHome::some(dir.path().to_path_buf());
    let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-autowrite")]);
    let args = LoadArgs {
        config: Some(&topology_path),
        auth: Some(&auth_path),
        model: None,
    };
    let config = load_with(args, &home, &env).expect("auto-write + load succeeds");

    assert!(
        auth_path.is_file(),
        "ensure_auth_file should have written the auth file"
    );
    let written = std::fs::read_to_string(&auth_path).expect("read back auth file");
    assert!(
        written.contains("[providers.anthropic]"),
        "auto-written file must contain the anthropic provider section: {written:?}"
    );
    // The template's `[models.default]` survives load_with because
    // ANTHROPIC_API_KEY was set in the env.
    assert_eq!(config.agents.len(), 1);
    assert_eq!(config.agents[0].model.api_key, "sk-autowrite");
}

#[test]
fn embedded_default_topology_has_five_builtin_tools() -> Result<(), Box<dyn std::error::Error>> {
    let topology: TopologyConfig =
        toml::from_str(DEFAULT_TOPOLOGY).expect("embedded default.toml parses");

    let mut keys: Vec<&str> = topology.tools.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["edit", "exec_bash", "insert", "read", "write"]);

    for expected in ["read", "write", "edit", "insert", "exec_bash"] {
        let tool = topology
            .tools
            .get(expected)
            .ok_or_else(|| format!("embedded topology declares `{expected}` tool"))?;
        match tool {
            ToolConfig::Builtin { builtin } => assert_eq!(
                builtin, expected,
                "tool `{expected}` registers built-in named `{expected}`"
            ),
        }
    }
    Ok(())
}

#[test]
fn embedded_default_topology_has_default_agent_with_five_tools() {
    let topology: TopologyConfig =
        toml::from_str(DEFAULT_TOPOLOGY).expect("embedded default.toml parses");

    assert_eq!(
        topology.agents.len(),
        1,
        "embedded topology has exactly one agent"
    );
    let agent = &topology.agents[0];
    assert_eq!(agent.name, "default");
    assert_eq!(agent.model, "default");
    assert_eq!(
        agent.tools,
        vec![
            "read".to_string(),
            "write".to_string(),
            "edit".to_string(),
            "insert".to_string(),
            "exec_bash".to_string(),
        ]
    );
    assert!(
        agent.system_prompt.len() > 100,
        "system prompt is a real coding-assistant prompt, not a placeholder \
         (got {} chars)",
        agent.system_prompt.len()
    );
}

fn auth_with_two_aliases() -> &'static str {
    r#"
[providers.anthropic]
type = "anthropic"

[models.default]
provider = "anthropic"
model_name = "claude-sonnet-4-6"
api_key = "{{ANTHROPIC_API_KEY}}"

[models.fast]
provider = "anthropic"
model_name = "claude-haiku"
api_key = "{{ANTHROPIC_API_KEY}}"
"#
}

#[test]
fn assemble_config_model_override_selects_existing_alias() {
    let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-foo")]);
    let config = assemble_config(
        valid_topology(),
        Path::new(TOPOLOGY_PATH),
        auth_with_two_aliases(),
        Path::new(AUTH_PATH),
        &env,
        Some("fast"),
    )
    .expect("override of an existing alias assembles");

    assert_eq!(config.agents.len(), 1);
    let agent = &config.agents[0];
    assert_eq!(agent.model.alias, "fast");
    assert_eq!(agent.model.model_name, "claude-haiku");
}

#[test]
fn assemble_config_model_override_absent_alias_dangles() -> Result<(), Box<dyn std::error::Error>> {
    let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-foo")]);
    let errors = assemble_config(
        valid_topology(),
        Path::new(TOPOLOGY_PATH),
        valid_auth(),
        Path::new(AUTH_PATH),
        &env,
        Some("absent"),
    )
    .expect_err("override of an absent alias should error");

    let mut saw_dangling = false;
    for err in errors.as_slice() {
        if let ConfigError::DanglingAlias { alias } = err {
            assert_eq!(alias, "absent");
            saw_dangling = true;
        }
    }
    assert!(
        saw_dangling,
        "expected DanglingAlias for the absent override alias; got {errors}"
    );
    Ok(())
}

#[test]
fn assemble_config_no_model_override_uses_declared_alias() {
    let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-foo")]);
    let config = assemble_config(
        valid_topology(),
        Path::new(TOPOLOGY_PATH),
        auth_with_two_aliases(),
        Path::new(AUTH_PATH),
        &env,
        None,
    )
    .expect("no override assembles");

    assert_eq!(config.agents.len(), 1);
    assert_eq!(config.agents[0].model.alias, "default");
}

fn topology_with_skills(skills_block: &str) -> String {
    format!(
        r#"
[[agents]]
name = "default"
model = "default"
system_prompt = "you are pristine"
tools = ["read"]

[tools.read]
type = "builtin"
builtin = "read"

{skills_block}
"#
    )
}

fn assemble_skills(skills_block: &str) -> Config {
    let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-foo")]);
    let topology = topology_with_skills(skills_block);
    assemble_config(
        &topology,
        Path::new(TOPOLOGY_PATH),
        valid_auth(),
        Path::new(AUTH_PATH),
        &env,
        None,
    )
    .expect("skills topology assembles")
}

#[test]
fn assemble_config_no_skills_block_disabled() {
    let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-foo")]);
    let config = assemble_config(
        valid_topology(),
        Path::new(TOPOLOGY_PATH),
        valid_auth(),
        Path::new(AUTH_PATH),
        &env,
        None,
    )
    .expect("no-skills topology assembles");
    assert!(
        config.skills.is_none(),
        "absent [skills] block resolves to None (disabled)"
    );
}

#[test]
fn assemble_config_skills_block_omitting_enabled_is_enabled() {
    // THE KEY TEST: a present [skills] block that omits `enabled` must resolve
    // to ENABLED (`Some`) even though it only sets unrelated fields.
    let config = assemble_skills("[skills]\nuser_paths = [\"~/custom\"]");
    assert!(
        config.skills.is_some(),
        "present block omitting `enabled` resolves to Some (enabled)"
    );
}

#[test]
fn assemble_config_empty_skills_block_is_enabled() {
    let config = assemble_skills("[skills]");
    assert!(
        config.skills.is_some(),
        "present empty block resolves to Some (enabled)"
    );
}

#[test]
fn assemble_config_skills_enabled_false_is_disabled() {
    let config = assemble_skills("[skills]\nenabled = false");
    assert!(
        config.skills.is_none(),
        "explicit enabled = false is the kill-switch (None)"
    );
}

#[test]
fn assemble_config_skills_enabled_true_is_enabled() {
    let config = assemble_skills("[skills]\nenabled = true");
    assert!(config.skills.is_some());
}

#[test]
fn assemble_config_skills_custom_paths_replace_defaults() {
    let config = assemble_skills("[skills]\nuser_paths = [\"~/a\"]\nproject_paths = [\"./b\"]");
    let skills = config.skills.expect("present block resolves to Some");
    assert_eq!(skills.effective_user_paths(), vec!["~/a".to_string()]);
    assert_eq!(skills.effective_project_paths(), vec!["./b".to_string()]);
}

#[test]
fn assemble_config_skills_rejects_unknown_field() {
    let env = MapEnv::new([("ANTHROPIC_API_KEY", "sk-foo")]);
    let topology = topology_with_skills("[skills]\nbogus = 1");
    let errors = assemble_config(
        &topology,
        Path::new(TOPOLOGY_PATH),
        valid_auth(),
        Path::new(AUTH_PATH),
        &env,
        None,
    )
    .expect_err("unknown skills field is rejected");
    let mut saw_parse = false;
    for err in errors.as_slice() {
        if let ConfigError::TomlParse { .. } = err {
            saw_parse = true;
        }
    }
    assert!(
        saw_parse,
        "expected a TomlParse error for the unknown skills field; got {errors}"
    );
}

#[test]
fn load_with_missing_home_when_default_auth_path_used() {
    let home = MockHome::none();
    let env = MapEnv::default();
    let args = LoadArgs::new();
    let errors = load_with(args, &home, &env).expect_err("missing home with default auth fails");
    let mut saw_missing_home = false;
    for err in errors.as_slice() {
        if matches!(err, ConfigError::MissingHome) {
            saw_missing_home = true;
        }
    }
    assert!(
        saw_missing_home,
        "expected ConfigError::MissingHome; got {errors}"
    );
}
