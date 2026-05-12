use futures::StreamExt;
use pristine::history::{Block, UserId};
use pristine::model::anthropic::AnthropicModelBuilder;
use pristine::model::{ARModel, ModelStreamEvent};
use std::env;
use std::time::Duration;

#[tokio::test]
#[ignore = "live API; run with `cargo nextest run --run-ignored only` and ANTHROPIC_API_KEY set"]
async fn live_anthropic_smoke() {
    let api_key = match env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("ANTHROPIC_API_KEY not set; skipping live test");
            return;
        }
    };

    let model = AnthropicModelBuilder::new()
        .api_key(api_key)
        .model_name("claude-haiku-4-5-20251001")
        .build()
        .expect("builder should succeed");

    let block = Block::UserMessage {
        from: UserId::new(),
        content: "ping".to_string(),
        timestamp: std::time::SystemTime::now(),
    };
    let messages = vec![block];

    let mut stream = model.complete("You are a terse assistant. Reply in one word.", &messages);

    let mut got_delta = false;
    let test_timeout = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(test_timeout);

    loop {
        tokio::select! {
            _ = &mut test_timeout => panic!("timed out waiting for content"),
            evt = stream.next() => match evt {
                Some(Ok(ModelStreamEvent::ContentDelta { .. })) => {
                    got_delta = true;
                }
                Some(Ok(ModelStreamEvent::MessageComplete { .. })) => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => panic!("model error: {e:?}"),
                None => break,
            }
        }
    }
    assert!(got_delta, "expected at least one ContentDelta");
}
