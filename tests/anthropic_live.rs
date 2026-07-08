use futures::StreamExt;
use pristine::model::anthropic::AnthropicProvider;
use pristine::model::{ContentPart, ModelInput, ModelStreamEvent, Role, ToolSpec, Turn};
use pristine::provider::{ModelInstanceConfig, ModelProvider};
use std::time::Duration;

#[tokio::test]
#[ignore = "live API; run with `cargo nextest run --run-ignored only` and ANTHROPIC_API_KEY set"]
async fn live_anthropic_smoke() -> Result<(), Box<dyn std::error::Error>> {
    let Some(api_key) = pristine::test_support::anthropic_key_or_skip() else {
        return Ok(());
    };

    let model = AnthropicProvider::new()
        .build_model(ModelInstanceConfig::new(
            "claude-haiku-4-5-20251001",
            serde_json::json!({ "api_key": api_key }),
        ))
        .expect("provider should build model");

    let input = ModelInput {
        turns: vec![
            Turn {
                role: Role::System,
                content: vec![ContentPart::Text(
                    "You are a terse assistant. Reply in one word.".to_string(),
                )],
            },
            Turn {
                role: Role::User,
                content: vec![ContentPart::Text("ping".to_string())],
            },
        ],
        tools: Vec::new(),
    };

    let mut stream = model.complete(&input);

    let mut got_delta = false;
    let test_timeout = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(test_timeout);

    loop {
        tokio::select! {
            _ = &mut test_timeout => return Err("timed out waiting for content".into()),
            evt = stream.next() => match evt {
                Some(Ok(ModelStreamEvent::ContentDelta { .. })) => {
                    got_delta = true;
                }
                Some(Ok(ModelStreamEvent::MessageComplete { .. })) => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("model error: {e:?}").into()),
                None => break,
            }
        }
    }
    assert!(got_delta, "expected at least one ContentDelta");

    Ok(())
}

#[tokio::test]
#[ignore = "live API; run with `cargo nextest run --run-ignored only` and ANTHROPIC_API_KEY set"]
async fn live_anthropic_tool_use_smoke() -> Result<(), Box<dyn std::error::Error>> {
    let Some(api_key) = pristine::test_support::anthropic_key_or_skip() else {
        return Ok(());
    };

    let model = AnthropicProvider::new()
        .build_model(ModelInstanceConfig::new(
            "claude-haiku-4-5-20251001",
            serde_json::json!({ "api_key": api_key }),
        ))
        .expect("provider should build model");

    let add_spec = ToolSpec {
        name: "add".to_string(),
        description: "Add two numbers and return their sum.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "a": {"type": "number"},
                "b": {"type": "number"}
            },
            "required": ["a", "b"]
        }),
    };

    let input = ModelInput {
        turns: vec![
            Turn {
                role: Role::System,
                content: vec![ContentPart::Text(
                    "You are a terse assistant. When arithmetic is asked, use the `add` tool. Reply in one word otherwise."
                        .to_string(),
                )],
            },
            Turn {
                role: Role::User,
                content: vec![ContentPart::Text(
                    "What is 17 plus 25? Use the add tool.".to_string(),
                )],
            },
        ],
        tools: vec![add_spec],
    };

    let mut stream = model.complete(&input);

    let mut saw_tool_use_start = false;
    let mut tool_use_complete_input: Option<serde_json::Value> = None;
    let test_timeout = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(test_timeout);

    loop {
        tokio::select! {
            _ = &mut test_timeout => return Err("timed out waiting for tool_use events".into()),
            evt = stream.next() => match evt {
                Some(Ok(ModelStreamEvent::ToolUseStart { name, .. })) => {
                    assert_eq!(name, "add", "expected tool_use_start with name 'add'");
                    saw_tool_use_start = true;
                }
                Some(Ok(ModelStreamEvent::ToolUseComplete { name, input, .. })) => {
                    assert_eq!(name, "add");
                    tool_use_complete_input = Some(input);
                }
                Some(Ok(ModelStreamEvent::MessageComplete { .. })) => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("model error: {e:?}").into()),
                None => break,
            }
        }
    }

    assert!(
        saw_tool_use_start,
        "expected at least one ToolUseStart with name=add"
    );
    let parsed = tool_use_complete_input.expect("expected a ToolUseComplete event");
    assert!(
        parsed.get("a").is_some() && parsed.get("b").is_some(),
        "expected ToolUseComplete.input to have fields 'a' and 'b', got {parsed:?}"
    );

    Ok(())
}
