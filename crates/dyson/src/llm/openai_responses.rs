// ===========================================================================
// OpenAI Responses API — the wire format Dyson uses for OpenAI's own models.
//
// OpenAI's newest models are only fully usable through `/v1/responses`:
// GPT-6 on `/v1/chat/completions` refuses function tools while reasoning, so
// an agent would have to give reasoning up. For an OpenAI model (`openai/…`)
// the OpenAI-compatible client speaks Responses instead, end to end: Swarm
// passes a Responses call straight to OpenAI when the model's route goes
// there, and translates it only for a hop that speaks Chat Completions.
//
// The request is built from the same Chat-shaped JSON the client already
// produces (tool names fitted by `wire_tools`, history encoded), so both wire
// formats share one message serialiser; this module only changes the shape.
// ===========================================================================

use serde_json::{Map, Value, json};

use super::sse_parser::{SseJsonParser, ToolBufferContext};
use super::stream::{StopReason, StreamEvent};
use dyson_core::error::{DysonError, Result};

/// Whether `model` is OpenAI's own and should go over Responses. Only
/// namespaced ids (`openai/gpt-6-luna`): that is how Swarm's catalogue and
/// routes name them, and a bare id on some other OpenAI-compatible server
/// keeps the Chat Completions it was configured for. Variants
/// (`openai/gpt-oss-120b`, `:free`) are other vendors' hosting and stay on
/// Chat Completions.
pub(crate) fn uses_responses(model: &str) -> bool {
    let Some(name) = model.strip_prefix("openai/") else {
        return false;
    };
    !name.contains(':')
        && !name.starts_with("gpt-oss")
        && (name.starts_with("gpt-")
            || name.starts_with("chatgpt-")
            || name.starts_with("codex-")
            || name.as_bytes().first() == Some(&b'o')
                && name.as_bytes().get(1).is_some_and(u8::is_ascii_digit))
}

/// A Chat Completions request body as a Responses request body.
pub(crate) fn request(chat: &Value) -> Value {
    let mut out = Map::new();
    if let Some(model) = chat.get("model") {
        out.insert("model".into(), model.clone());
    }
    let mut input = Vec::new();
    for message in chat
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        push_message(message, &mut input);
    }
    out.insert("input".into(), Value::Array(input));
    let tools: Vec<Value> = chat
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(tool)
        .collect();
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
    }
    if let Some(max) = chat
        .get("max_completion_tokens")
        .or_else(|| chat.get("max_tokens"))
    {
        out.insert("max_output_tokens".into(), max.clone());
    }
    if let Some(temperature) = chat.get("temperature") {
        out.insert("temperature".into(), temperature.clone());
    }
    out.insert(
        "stream".into(),
        Value::Bool(chat.get("stream").and_then(Value::as_bool) == Some(true)),
    );
    // Dyson keeps the transcript; nothing needs to stay at OpenAI.
    out.insert("store".into(), Value::Bool(false));
    Value::Object(out)
}

fn push_message(message: &Value, input: &mut Vec<Value>) {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user");
    match role {
        "tool" => input.push(json!({
            "type": "function_call_output",
            "call_id": message.get("tool_call_id").cloned().unwrap_or(Value::Null),
            "output": plain_text(message.get("content")),
        })),
        "assistant" => {
            let text = plain_text(message.get("content"));
            if !text.is_empty() {
                input.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": text }],
                }));
            }
            for call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let function = call.get("function").unwrap_or(&Value::Null);
                input.push(json!({
                    "type": "function_call",
                    "call_id": call.get("id").cloned().unwrap_or(Value::Null),
                    "name": function.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": match function.get("arguments") {
                        Some(Value::String(s)) => s.clone(),
                        Some(other) => other.to_string(),
                        None => "{}".to_owned(),
                    },
                }));
            }
        }
        _ => {
            let role = if role == "system" { "developer" } else { role };
            input.push(json!({
                "type": "message",
                "role": role,
                "content": input_parts(message.get("content")),
            }));
        }
    }
}

fn input_parts(content: Option<&Value>) -> Value {
    match content {
        Some(Value::Array(parts)) => Value::Array(
            parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("text") => Some(json!({
                        "type": "input_text",
                        "text": part.get("text").cloned().unwrap_or_default(),
                    })),
                    Some("image_url") => Some(json!({
                        "type": "input_image",
                        "image_url": part
                            .get("image_url")
                            .and_then(|i| i.get("url").or(Some(i)))
                            .cloned()
                            .unwrap_or_default(),
                    })),
                    _ => None,
                })
                .collect(),
        ),
        other => json!([{ "type": "input_text", "text": plain_text(other) }]),
    }
}

fn plain_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn tool(tool: &Value) -> Option<Value> {
    let function = tool.get("function")?;
    Some(json!({
        "type": "function",
        "name": function.get("name")?,
        "description": function.get("description").cloned().unwrap_or_default(),
        "parameters": function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
        "strict": false,
    }))
}

/// Responses SSE events → Dyson stream events.
#[derive(Default)]
pub(crate) struct ResponsesJsonParser {
    /// Function-call `item_id`s in arrival order; position = buffer index.
    calls: Vec<String>,
    completed: bool,
}

impl ResponsesJsonParser {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn index_of(&self, value: &Value) -> Option<usize> {
        let item_id = value.get("item_id").and_then(Value::as_str)?;
        self.calls.iter().position(|id| id == item_id)
    }
}

impl SseJsonParser for ResponsesJsonParser {
    fn parse_json(
        &mut self,
        json: &Value,
        ctx: &mut ToolBufferContext,
    ) -> Vec<Result<StreamEvent>> {
        let mut events = Vec::new();
        if self.completed {
            return events;
        }
        match json.get("type").and_then(Value::as_str).unwrap_or_default() {
            "response.output_text.delta" => {
                if let Some(text) = json.get("delta").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    events.push(Ok(StreamEvent::TextDelta(text.to_owned())));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(text) = json.get("delta").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    events.push(Ok(StreamEvent::ThinkingDelta(text.to_owned())));
                }
            }
            "response.output_item.added" => {
                let item = json.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) != Some("function_call") {
                    return events;
                }
                let index = self.calls.len();
                self.calls.push(
                    item.get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                );
                let id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if let Some(error) = ctx.start_tool(index, id.clone(), name.clone()) {
                    events.push(Err(DysonError::Llm(format!("{error:?}"))));
                    return events;
                }
                events.push(Ok(StreamEvent::ToolUseStart { id, name }));
                // Some servers put the whole argument string on the item.
                if let Some(args) = item.get("arguments").and_then(Value::as_str)
                    && !args.is_empty()
                {
                    match ctx.append_tool_json(index, args) {
                        Some(error) => events.push(Ok(error)),
                        None => events.push(Ok(StreamEvent::ToolUseInputDelta(args.to_owned()))),
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let Some(index) = self.index_of(json) else {
                    return events;
                };
                if let Some(args) = json.get("delta").and_then(Value::as_str) {
                    match ctx.append_tool_json(index, args) {
                        Some(error) => events.push(Ok(error)),
                        None => events.push(Ok(StreamEvent::ToolUseInputDelta(args.to_owned()))),
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
                self.completed = true;
                let response = json.get("response").unwrap_or(&Value::Null);
                let output_tokens = response
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                    .map(|n| n as usize);
                let incomplete =
                    response.get("status").and_then(Value::as_str) == Some("incomplete");
                let stop_reason = if !self.calls.is_empty() {
                    events.extend(ctx.drain_all());
                    StopReason::ToolUse
                } else if incomplete {
                    StopReason::MaxTokens
                } else {
                    StopReason::EndTurn
                };
                events.push(Ok(StreamEvent::MessageComplete {
                    stop_reason,
                    output_tokens,
                }));
            }
            "response.failed" | "error" => {
                self.completed = true;
                let error = json
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .or_else(|| json.get("error"))
                    .unwrap_or(json);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("the model's response failed");
                events.push(Err(DysonError::Llm(format!("OpenAI: {message}"))));
            }
            _ => {}
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::SseStreamParser;
    use crate::llm::sse_parser::BaseSseParser;

    #[test]
    fn only_openai_namespaced_models_use_responses() {
        for model in [
            "openai/gpt-6-luna",
            "openai/gpt-5.5",
            "openai/o3-mini",
            "openai/codex-mini",
        ] {
            assert!(uses_responses(model), "{model}");
        }
        for model in [
            "gpt-6-luna",
            "openai/gpt-oss-120b",
            "openai/gpt-5.5:free",
            "anthropic/claude-opus-4.8",
            "deepseek/deepseek-v4.1-flash",
            "openai/omni-moderation-latest",
        ] {
            assert!(!uses_responses(model), "{model}");
        }
    }

    #[test]
    fn a_chat_body_becomes_a_responses_body() {
        let r = request(&json!({
            "model": "openai/gpt-6-luna",
            "max_tokens": 16384,
            "stream": true,
            "messages": [
                {"role": "system", "content": "you are TARS"},
                {"role": "user", "content": "ping"},
                {"role": "assistant", "content": "checking", "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "more_tools", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "ok"},
            ],
            "tools": [{"type": "function", "function": {"name": "bash", "description": "run", "parameters": {"type": "object"}}}],
        }));
        assert_eq!(r["model"], "openai/gpt-6-luna");
        assert_eq!(r["max_output_tokens"], 16384);
        assert_eq!(r["store"], false);
        let input = r["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(
            input[2]["content"][0],
            json!({"type": "output_text", "text": "checking"})
        );
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(
            input[4],
            json!({"type": "function_call_output", "call_id": "call_1", "output": "ok"})
        );
        assert_eq!(r["tools"][0]["name"], "bash");
        assert!(r.get("messages").is_none());
    }

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
            .collect()
    }

    fn parse(text: &str) -> Vec<StreamEvent> {
        let mut parser = BaseSseParser::new(ResponsesJsonParser::new());
        // Fed in small slices, as the network delivers it.
        text.as_bytes()
            .chunks(11)
            .flat_map(|chunk| parser.feed(chunk))
            .map(|e| e.unwrap())
            .collect()
    }

    #[test]
    fn a_streamed_tool_call_reaches_the_agent() {
        let events = parse(&sse(&[
            json!({"type": "response.created", "response": {"id": "r"}}),
            json!({"type": "response.reasoning_summary_text.delta", "delta": "thinking"}),
            json!({"type": "response.output_text.delta", "delta": "On it"}),
            json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "bash", "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{\"command\":"}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "\"ls\"}"}),
            json!({"type": "response.completed", "response": {"status": "completed", "usage": {"output_tokens": 9}}}),
        ]));
        assert!(matches!(&events[0], StreamEvent::ThinkingDelta(t) if t == "thinking"));
        assert!(matches!(&events[1], StreamEvent::TextDelta(t) if t == "On it"));
        assert!(
            matches!(&events[2], StreamEvent::ToolUseStart { id, name } if id == "call_1" && name == "bash")
        );
        let complete = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::ToolUseComplete { name, input, .. } => {
                    Some((name.clone(), input.clone()))
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(complete, ("bash".to_owned(), json!({"command": "ls"})));
        assert!(matches!(
            events.last(),
            Some(StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                output_tokens: Some(9)
            })
        ));
    }

    #[test]
    fn text_only_and_truncated_answers_end_correctly() {
        let done = parse(&sse(&[
            json!({"type": "response.output_text.delta", "delta": "Pong"}),
            json!({"type": "response.completed", "response": {"status": "completed"}}),
        ]));
        assert!(matches!(
            done.last(),
            Some(StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                ..
            })
        ));

        let cut = parse(&sse(&[
            json!({"type": "response.incomplete", "response": {"status": "incomplete"}}),
        ]));
        assert!(matches!(
            cut.last(),
            Some(StreamEvent::MessageComplete {
                stop_reason: StopReason::MaxTokens,
                ..
            })
        ));
    }

    #[test]
    fn a_failed_response_is_an_error() {
        let mut parser = BaseSseParser::new(ResponsesJsonParser::new());
        let events = parser.feed(
            sse(&[json!({"type": "response.failed", "response": {"error": {"message": "boom"}}})])
                .as_bytes(),
        );
        match &events[0] {
            Err(DysonError::Llm(message)) => assert!(message.contains("boom")),
            other => panic!("{other:?}"),
        }
    }
}
