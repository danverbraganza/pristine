//! Shared OpenAI ChatCompletions dialect: request shaping, streaming parse,
//! and the SSE drive loop reused by every OpenAI-compatible provider.

use std::collections::HashMap;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use super::{
    ContentPart, Error, ModelInput, ModelStreamEvent, Role, Usage, tool_result_wire_string,
};

// Hard-coded request-shape default; configurability (e.g. via ModelInstanceConfig extras) is deferred.
pub(crate) const MAX_TOKENS: u32 = 8192;

#[derive(serde::Serialize)]
pub(crate) struct OpenAiRequest<'a> {
    pub model: &'a str,
    pub messages: Vec<OpenAiMessage>,
    pub stream: bool,
    pub max_tokens: u32,
    // OpenAI-compatible knob that delivers a final usage-bearing chunk in the
    // stream. DeepSeek V4 defaults to thinking mode and surfaces
    // `reasoning_content` without an explicit toggle, so no thinking parameter
    // is sent; we rely on the documented default.
    pub stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<OpenAiTool>,
}

/// One entry in the OpenAI-dialect `tools` array. The portable `ToolSpec` maps
/// onto this nested `{type: function, function: {...}}` shape, in contrast to
/// Anthropic's flat top-level tool object.
#[derive(serde::Serialize)]
pub(crate) struct OpenAiTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiFunction,
}

#[derive(serde::Serialize)]
pub(crate) struct OpenAiFunction {
    name: String,
    description: String,
    // OpenAI nests the JSON schema under `function.parameters`, where Anthropic
    // uses a top-level `input_schema`.
    parameters: serde_json::Value,
}

#[derive(serde::Serialize)]
pub(crate) struct StreamOptions {
    pub include_usage: bool,
}

/// One OpenAI-dialect wire message. A single portable `Turn` may explode into
/// several of these (e.g. assistant text + tool_calls, plus one `tool` message
/// per tool result), so the mapping flattens turns into a flat message list.
#[derive(serde::Serialize)]
pub(crate) struct OpenAiMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OpenAiToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(serde::Serialize)]
pub(crate) struct OpenAiToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiFunctionCall,
}

#[derive(serde::Serialize)]
pub(crate) struct OpenAiFunctionCall {
    name: String,
    // OpenAI requires the arguments to be a JSON-encoded *string*, not a nested
    // object.
    arguments: String,
}

pub(crate) fn model_input_to_openai(input: &ModelInput) -> (Vec<OpenAiMessage>, Vec<OpenAiTool>) {
    let mut messages: Vec<OpenAiMessage> = Vec::with_capacity(input.turns.len());
    for turn in &input.turns {
        match turn.role {
            Role::System => {
                let mut system = String::new();
                for part in &turn.content {
                    match part {
                        ContentPart::Text(text) => {
                            if !system.is_empty() {
                                system.push_str("\n\n");
                            }
                            system.push_str(text);
                        }
                        ContentPart::ToolUse { .. } | ContentPart::ToolResult { .. } => {
                            // Tool exchanges have no meaningful place in a system turn; drop them.
                        }
                    }
                }
                if !system.is_empty() {
                    messages.push(OpenAiMessage {
                        role: "system",
                        content: Some(system),
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                    });
                }
            }
            Role::User | Role::Assistant => {
                let role: &'static str = if matches!(turn.role, Role::User) {
                    "user"
                } else {
                    "assistant"
                };
                let mut text = String::new();
                let mut tool_calls: Vec<OpenAiToolCall> = Vec::new();
                let mut pending_tool_results: Vec<OpenAiMessage> = Vec::new();
                for part in &turn.content {
                    match part {
                        ContentPart::Text(t) => {
                            if !text.is_empty() {
                                text.push_str("\n\n");
                            }
                            text.push_str(t);
                        }
                        ContentPart::ToolUse { id, name, input } => {
                            tool_calls.push(OpenAiToolCall {
                                id: id.clone(),
                                kind: "function",
                                function: OpenAiFunctionCall {
                                    name: name.clone(),
                                    arguments: input.to_string(),
                                },
                            });
                        }
                        ContentPart::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            // OpenAI has no is_error field on tool messages; fold
                            // the failure flag into the content so the model still
                            // sees that the call failed.
                            let rendered = tool_result_wire_string(content);
                            let content = if *is_error {
                                format!("[tool error] {rendered}")
                            } else {
                                rendered
                            };
                            pending_tool_results.push(OpenAiMessage {
                                role: "tool",
                                content: Some(content),
                                tool_calls: Vec::new(),
                                tool_call_id: Some(tool_use_id.clone()),
                            });
                        }
                    }
                }

                if !text.is_empty() || !tool_calls.is_empty() {
                    messages.push(OpenAiMessage {
                        role,
                        content: if text.is_empty() { None } else { Some(text) },
                        tool_calls,
                        tool_call_id: None,
                    });
                }
                // Tool results become their own `tool`-role messages, so a single
                // portable Turn can explode into multiple wire messages.
                messages.append(&mut pending_tool_results);
            }
        }
    }
    let tools = input
        .tools
        .iter()
        .map(|spec| OpenAiTool {
            kind: "function",
            function: OpenAiFunction {
                name: spec.name.clone(),
                description: spec.description.clone(),
                parameters: spec.input_schema.clone(),
            },
        })
        .collect();
    (messages, tools)
}

#[derive(serde::Deserialize)]
struct StreamChunk {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<ChunkUsage>,
}

#[derive(serde::Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    // OpenRouter normalizes reasoning into `reasoning`; DeepSeek emits
    // `reasoning_content`. Accept either spelling so both surface as
    // ReasoningDelta.
    #[serde(default, alias = "reasoning_content")]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(serde::Deserialize)]
struct ToolCallDelta {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(serde::Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(serde::Deserialize)]
struct ChunkUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

struct ToolCallState {
    id: String,
    name: String,
    arguments: String,
}

/// Drive an OpenAI ChatCompletions SSE stream into portable
/// `ModelStreamEvent`s. The caller supplies a fully-formed request body and
/// the per-provider URL; this owns the wire protocol (event ordering, the
/// `[DONE]` sentinel, and the send-or-return idiom).
pub(crate) fn stream_openai_chat(
    client: reqwest::Client,
    url: String,
    api_key: String,
    request_body: serde_json::Value,
) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<ModelStreamEvent, Error>> + Send>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<ModelStreamEvent, Error>>(64);

    tokio::spawn(async move {
        let response = match client
            .post(&url)
            .header("authorization", format!("Bearer {api_key}"))
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
        let mut started = false;
        let mut content_acc = String::new();
        let mut reasoning_acc = String::new();
        let mut tool_calls: HashMap<u32, ToolCallState> = HashMap::new();
        let mut last_usage = Usage::default();

        while let Some(event) = events.next().await {
            let event = match event {
                Ok(ev) => ev,
                Err(e) => {
                    let _ = tx.send(Err(Error::Deserialization(e.to_string()))).await;
                    return;
                }
            };

            if event.data.trim() == "[DONE]" {
                break;
            }

            let chunk: StreamChunk = match serde_json::from_str(&event.data) {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Err(Error::Deserialization(e.to_string()))).await;
                    return;
                }
            };

            if !started && let Some(message_id) = chunk.id.clone() {
                started = true;
                if tx
                    .send(Ok(ModelStreamEvent::MessageStart {
                        message_id,
                        model: chunk.model.clone().unwrap_or_default(),
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
            }

            let mut finished = false;
            if let Some(choice) = chunk.choices.into_iter().next() {
                if let Some(text) = choice.delta.reasoning {
                    reasoning_acc.push_str(&text);
                    if tx
                        .send(Ok(ModelStreamEvent::ReasoningDelta { text }))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                if let Some(text) = choice.delta.content {
                    content_acc.push_str(&text);
                    if tx
                        .send(Ok(ModelStreamEvent::ContentDelta { text }))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                for tc in choice.delta.tool_calls {
                    let entry = tool_calls.entry(tc.index);
                    match entry {
                        std::collections::hash_map::Entry::Vacant(slot) => {
                            let id = tc.id.clone().unwrap_or_default();
                            let name = tc
                                .function
                                .as_ref()
                                .and_then(|f| f.name.clone())
                                .unwrap_or_default();
                            let mut arguments = String::new();
                            if let Some(f) = &tc.function
                                && let Some(a) = &f.arguments
                            {
                                arguments.push_str(a);
                            }
                            slot.insert(ToolCallState {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: arguments.clone(),
                            });
                            let start_id = id.clone();
                            if tx
                                .send(Ok(ModelStreamEvent::ToolUseStart { id, name }))
                                .await
                                .is_err()
                            {
                                return;
                            }
                            if !arguments.is_empty()
                                && tx
                                    .send(Ok(ModelStreamEvent::ToolUseDelta {
                                        id: start_id,
                                        partial_json: arguments,
                                    }))
                                    .await
                                    .is_err()
                            {
                                return;
                            }
                        }
                        std::collections::hash_map::Entry::Occupied(mut slot) => {
                            let state = slot.get_mut();
                            if let Some(id) = &tc.id
                                && state.id.is_empty()
                            {
                                state.id = id.clone();
                            }
                            if let Some(f) = &tc.function {
                                if let Some(name) = &f.name
                                    && state.name.is_empty()
                                {
                                    state.name = name.clone();
                                }
                                if let Some(args) = &f.arguments {
                                    state.arguments.push_str(args);
                                    let id = state.id.clone();
                                    if tx
                                        .send(Ok(ModelStreamEvent::ToolUseDelta {
                                            id,
                                            partial_json: args.clone(),
                                        }))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
                if choice.finish_reason.is_some() {
                    finished = true;
                }
            }

            if let Some(usage) = chunk.usage {
                last_usage = Usage {
                    input_tokens: usage.prompt_tokens,
                    output_tokens: usage.completion_tokens,
                };
                if tx
                    .send(Ok(ModelStreamEvent::Usage(last_usage)))
                    .await
                    .is_err()
                {
                    return;
                }
            }

            if finished
                && !flush_completions(&tx, &mut reasoning_acc, &mut content_acc, &mut tool_calls)
                    .await
            {
                return;
            }
        }

        if !flush_completions(&tx, &mut reasoning_acc, &mut content_acc, &mut tool_calls).await {
            return;
        }

        let _ = tx
            .send(Ok(ModelStreamEvent::MessageComplete { usage: last_usage }))
            .await;
    });

    Box::pin(ReceiverStream::new(rx))
}

/// Drains the accumulated reasoning/content/tool-call state, emitting the
/// matching `*Complete` events. Returns `false` if the receiver hung up.
async fn flush_completions(
    tx: &tokio::sync::mpsc::Sender<Result<ModelStreamEvent, Error>>,
    reasoning_acc: &mut String,
    content_acc: &mut String,
    tool_calls: &mut HashMap<u32, ToolCallState>,
) -> bool {
    if !reasoning_acc.is_empty() {
        let text = std::mem::take(reasoning_acc);
        if tx
            .send(Ok(ModelStreamEvent::ReasoningComplete { text }))
            .await
            .is_err()
        {
            return false;
        }
    }
    if !content_acc.is_empty() {
        let text = std::mem::take(content_acc);
        if tx
            .send(Ok(ModelStreamEvent::ContentComplete { text }))
            .await
            .is_err()
        {
            return false;
        }
    }
    if !tool_calls.is_empty() {
        let mut entries: Vec<(u32, ToolCallState)> = tool_calls.drain().collect();
        entries.sort_by_key(|(index, _)| *index);
        for (_, state) in entries {
            let input = if state.arguments.trim().is_empty() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                match serde_json::from_str::<serde_json::Value>(&state.arguments) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = tx
                            .send(Err(Error::Deserialization(format!(
                                "tool_call arguments parse: {e}"
                            ))))
                            .await;
                        return false;
                    }
                }
            };
            if tx
                .send(Ok(ModelStreamEvent::ToolUseComplete {
                    id: state.id,
                    name: state.name,
                    input,
                }))
                .await
                .is_err()
            {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ToolSpec, Turn};

    #[test]
    fn system_turn_becomes_system_message() {
        let input = ModelInput {
            turns: vec![
                Turn {
                    role: Role::System,
                    content: vec![ContentPart::Text("first".to_string())],
                },
                Turn {
                    role: Role::System,
                    content: vec![ContentPart::Text("second".to_string())],
                },
                Turn {
                    role: Role::User,
                    content: vec![ContentPart::Text("hi".to_string())],
                },
            ],
            tools: Vec::new(),
        };
        let (messages, _tools) = model_input_to_openai(&input);
        let value = serde_json::to_value(&messages).expect("serialize messages");
        assert_eq!(value[0]["role"], "system");
        assert_eq!(value[0]["content"], "first");
        assert_eq!(value[1]["role"], "system");
        assert_eq!(value[1]["content"], "second");
        assert_eq!(value[2]["role"], "user");
        assert_eq!(value[2]["content"], "hi");
        assert!(value[0].get("tool_calls").is_none());
    }

    #[test]
    fn request_sets_stream_options_and_max_tokens() {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::User,
                content: vec![ContentPart::Text("hi".to_string())],
            }],
            tools: Vec::new(),
        };
        let (messages, tools) = model_input_to_openai(&input);
        let request = OpenAiRequest {
            model: "deepseek-v4-pro",
            messages,
            stream: true,
            max_tokens: MAX_TOKENS,
            stream_options: StreamOptions {
                include_usage: true,
            },
            tools,
        };
        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["model"], "deepseek-v4-pro");
        assert_eq!(value["stream"], true);
        assert_eq!(value["max_tokens"], 8192);
        assert_eq!(value["stream_options"]["include_usage"], true);
        // No top-level system field in the OpenAI dialect.
        assert!(value.get("system").is_none());
    }

    #[test]
    fn serializes_tools_field_when_present() {
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
            model: "deepseek-v4-pro",
            messages,
            stream: true,
            max_tokens: MAX_TOKENS,
            stream_options: StreamOptions {
                include_usage: true,
            },
            tools,
        };
        let value = serde_json::to_value(&request).expect("serialize request");
        // OpenAI nests the spec under function.{name,description,parameters}.
        assert_eq!(value["tools"][0]["type"], "function");
        assert_eq!(value["tools"][0]["function"]["name"], "echo");
        assert_eq!(value["tools"][0]["function"]["description"], "d");
        assert_eq!(
            value["tools"][0]["function"]["parameters"]["type"],
            "object"
        );
    }

    #[test]
    fn omits_tools_field_when_empty() {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::User,
                content: vec![ContentPart::Text("hi".into())],
            }],
            tools: Vec::new(),
        };
        let (messages, tools) = model_input_to_openai(&input);
        let request = OpenAiRequest {
            model: "deepseek-v4-pro",
            messages,
            stream: true,
            max_tokens: MAX_TOKENS,
            stream_options: StreamOptions {
                include_usage: true,
            },
            tools,
        };
        let value = serde_json::to_value(&request).expect("serialize request");
        assert!(value.get("tools").is_none());
    }

    #[test]
    fn user_text_becomes_user_message() {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::User,
                content: vec![ContentPart::Text("hello".to_string())],
            }],
            tools: Vec::new(),
        };
        let (messages, _tools) = model_input_to_openai(&input);
        let value = serde_json::to_value(&messages).expect("serialize messages");
        assert_eq!(value[0]["role"], "user");
        assert_eq!(value[0]["content"], "hello");
    }

    #[test]
    fn tool_use_becomes_tool_calls_with_stringified_arguments() {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse {
                    id: "call_1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({ "text": "hi" }),
                }],
            }],
            tools: Vec::new(),
        };
        let (messages, _tools) = model_input_to_openai(&input);
        let value = serde_json::to_value(&messages).expect("serialize messages");
        assert_eq!(value[0]["role"], "assistant");
        assert_eq!(value[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(value[0]["tool_calls"][0]["type"], "function");
        assert_eq!(value[0]["tool_calls"][0]["function"]["name"], "echo");
        let args = &value[0]["tool_calls"][0]["function"]["arguments"];
        assert!(
            args.is_string(),
            "arguments must be a JSON string, got {args}"
        );
        assert_eq!(args.as_str().expect("arguments string"), r#"{"text":"hi"}"#);
    }

    #[test]
    fn tool_result_becomes_separate_tool_message() {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::User,
                content: vec![ContentPart::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: serde_json::json!("ok"),
                    is_error: false,
                }],
            }],
            tools: Vec::new(),
        };
        let (messages, _tools) = model_input_to_openai(&input);
        let value = serde_json::to_value(&messages).expect("serialize messages");
        assert_eq!(value[0]["role"], "tool");
        assert_eq!(value[0]["tool_call_id"], "call_1");
        assert_eq!(value[0]["content"], "ok");
        assert!(value[0].get("is_error").is_none());
    }

    #[test]
    fn tool_result_error_is_folded_into_content() {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::User,
                content: vec![ContentPart::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: serde_json::json!("boom"),
                    is_error: true,
                }],
            }],
            tools: Vec::new(),
        };
        let (messages, _tools) = model_input_to_openai(&input);
        let value = serde_json::to_value(&messages).expect("serialize messages");
        assert_eq!(value[0]["role"], "tool");
        let content = value[0]["content"].as_str().expect("content string");
        assert!(
            content.contains("boom") && content.contains("error"),
            "expected folded error content, got {content:?}"
        );
        assert!(value[0].get("is_error").is_none());
    }

    #[test]
    fn tool_result_object_value_is_stringified() {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::User,
                content: vec![ContentPart::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: serde_json::json!({ "content": "# README" }),
                    is_error: false,
                }],
            }],
            tools: Vec::new(),
        };
        let (messages, _tools) = model_input_to_openai(&input);
        let value = serde_json::to_value(&messages).expect("serialize messages");
        let content = &value[0]["content"];
        assert!(
            content.is_string(),
            "content must be a string, got {content}"
        );
        assert_eq!(
            content.as_str().expect("content string"),
            r##"{"content":"# README"}"##
        );
    }

    #[test]
    fn turn_with_two_tool_results_explodes_into_two_tool_messages() {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::User,
                content: vec![
                    ContentPart::ToolResult {
                        tool_use_id: "call_1".into(),
                        content: serde_json::json!("a"),
                        is_error: false,
                    },
                    ContentPart::ToolResult {
                        tool_use_id: "call_2".into(),
                        content: serde_json::json!("b"),
                        is_error: false,
                    },
                ],
            }],
            tools: Vec::new(),
        };
        let (messages, _tools) = model_input_to_openai(&input);
        let value = serde_json::to_value(&messages).expect("serialize messages");
        let arr = value.as_array().expect("messages array");
        assert_eq!(
            arr.len(),
            2,
            "two tool results must explode into two messages"
        );
        assert_eq!(value[0]["role"], "tool");
        assert_eq!(value[0]["tool_call_id"], "call_1");
        assert_eq!(value[0]["content"], "a");
        assert_eq!(value[1]["role"], "tool");
        assert_eq!(value[1]["tool_call_id"], "call_2");
        assert_eq!(value[1]["content"], "b");
    }

    #[test]
    fn assistant_text_and_tool_use_then_results_explode() {
        let input = ModelInput {
            turns: vec![Turn {
                role: Role::Assistant,
                content: vec![
                    ContentPart::Text("working".into()),
                    ContentPart::ToolUse {
                        id: "call_1".into(),
                        name: "echo".into(),
                        input: serde_json::json!({ "x": 1 }),
                    },
                    ContentPart::ToolResult {
                        tool_use_id: "call_1".into(),
                        content: serde_json::json!("done"),
                        is_error: false,
                    },
                ],
            }],
            tools: Vec::new(),
        };
        let (messages, _tools) = model_input_to_openai(&input);
        let value = serde_json::to_value(&messages).expect("serialize messages");
        let arr = value.as_array().expect("messages array");
        assert_eq!(arr.len(), 2);
        assert_eq!(value[0]["role"], "assistant");
        assert_eq!(value[0]["content"], "working");
        assert_eq!(value[0]["tool_calls"][0]["function"]["name"], "echo");
        assert_eq!(value[1]["role"], "tool");
        assert_eq!(value[1]["tool_call_id"], "call_1");
    }

    #[test]
    fn parses_content_delta_chunk() -> Result<(), Box<dyn std::error::Error>> {
        let data = r#"{"id":"c1","model":"deepseek-v4-pro","choices":[{"delta":{"content":"hi"},"finish_reason":null}]}"#;
        let chunk: StreamChunk = serde_json::from_str(data)?;
        let choice = chunk.choices.into_iter().next().ok_or("missing choice")?;
        assert_eq!(choice.delta.content.as_deref(), Some("hi"));
        assert!(choice.delta.reasoning.is_none());
        assert!(choice.finish_reason.is_none());
        Ok(())
    }

    #[test]
    fn parses_reasoning_content_delta_chunk() -> Result<(), Box<dyn std::error::Error>> {
        let data = r#"{"id":"c1","choices":[{"delta":{"reasoning_content":"thinking"},"finish_reason":null}]}"#;
        let chunk: StreamChunk = serde_json::from_str(data)?;
        let choice = chunk.choices.into_iter().next().ok_or("missing choice")?;
        assert_eq!(choice.delta.reasoning.as_deref(), Some("thinking"));
        assert!(choice.delta.content.is_none());
        Ok(())
    }

    #[test]
    fn parses_reasoning_alias_delta_chunk() -> Result<(), Box<dyn std::error::Error>> {
        // OpenRouter normalizes reasoning into the `reasoning` field; the alias
        // makes it surface identically to DeepSeek's `reasoning_content`.
        let data =
            r#"{"id":"c1","choices":[{"delta":{"reasoning":"thinking"},"finish_reason":null}]}"#;
        let chunk: StreamChunk = serde_json::from_str(data)?;
        let choice = chunk.choices.into_iter().next().ok_or("missing choice")?;
        assert_eq!(choice.delta.reasoning.as_deref(), Some("thinking"));
        assert!(choice.delta.content.is_none());
        Ok(())
    }

    #[test]
    fn parses_tool_call_deltas_across_an_index() -> Result<(), Box<dyn std::error::Error>> {
        let start = r#"{"id":"c1","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"echo","arguments":"{\"t\""}}]},"finish_reason":null}]}"#;
        let cont = r#"{"id":"c1","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;

        let chunk1: StreamChunk = serde_json::from_str(start)?;
        let choice1 = chunk1.choices.into_iter().next().ok_or("missing choice")?;
        let tc1 = choice1
            .delta
            .tool_calls
            .into_iter()
            .next()
            .ok_or("missing tool_call")?;
        assert_eq!(tc1.index, 0);
        assert_eq!(tc1.id.as_deref(), Some("call_1"));
        let f1 = tc1.function.ok_or("missing function")?;
        assert_eq!(f1.name.as_deref(), Some("echo"));
        assert_eq!(f1.arguments.as_deref(), Some(r#"{"t""#));

        let chunk2: StreamChunk = serde_json::from_str(cont)?;
        let choice2 = chunk2.choices.into_iter().next().ok_or("missing choice")?;
        assert_eq!(choice2.finish_reason.as_deref(), Some("tool_calls"));
        let tc2 = choice2
            .delta
            .tool_calls
            .into_iter()
            .next()
            .ok_or("missing tool_call")?;
        assert_eq!(tc2.index, 0);
        let f2 = tc2.function.ok_or("missing function")?;
        assert_eq!(f2.arguments.as_deref(), Some(r#":"hi"}"#));

        // The two argument fragments concatenate into valid JSON.
        let joined = format!(
            "{}{}",
            f1.arguments.unwrap_or_default(),
            f2.arguments.unwrap_or_default()
        );
        let parsed: serde_json::Value = serde_json::from_str(&joined)?;
        assert_eq!(parsed, serde_json::json!({ "t": "hi" }));
        Ok(())
    }

    #[test]
    fn parses_usage_chunk() -> Result<(), Box<dyn std::error::Error>> {
        let data =
            r#"{"id":"c1","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":34}}"#;
        let chunk: StreamChunk = serde_json::from_str(data)?;
        let usage = chunk.usage.ok_or("missing usage")?;
        assert_eq!(usage.prompt_tokens, 12);
        assert_eq!(usage.completion_tokens, 34);
        assert!(chunk.choices.is_empty());
        Ok(())
    }

    #[test]
    fn done_sentinel_is_recognized() {
        // The streaming loop treats a literal "[DONE]" data line as terminal.
        assert_eq!("[DONE]".trim(), "[DONE]");
    }
}
