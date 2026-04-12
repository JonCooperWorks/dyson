// ===========================================================================
// OpenAI client — streaming SSE implementation of the Chat Completions API.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements the OpenAI Chat Completions API protocol (SSE streaming,
//   message serialization, tool call accumulation).  Used directly for
//   OpenAI proper, and wrapped by `OpenAiCompatClient` for other
//   OpenAI-compatible endpoints that may need dialect support.
//
// How OpenAI streaming differs from Anthropic:
//
//   Anthropic:
//     - Content blocks have explicit start/delta/stop lifecycle
//     - Tool input arrives as partial JSON strings
//     - Tool calls are part of the content array
//     - System prompt is a separate field
//
//   OpenAI:
//     - Responses arrive as "choices" with "delta" objects
//     - Tool calls arrive in delta.tool_calls[] with index-based accumulation
//     - Tool calls are a separate field from content, not content blocks
//     - System prompt is a message with role "system"
//     - The stream ends with a [DONE] sentinel
//
// SSE format (OpenAI):
//
//   data: {"id":"chatcmpl-...","object":"chat.completion.chunk","choices":[
//           {"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}
//
//   data: {"id":"chatcmpl-...","choices":[
//           {"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function",
//           "function":{"name":"bash","arguments":""}}]},"finish_reason":null}]}
//
//   data: {"id":"chatcmpl-...","choices":[
//           {"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"com"}}]},
//           "finish_reason":null}]}
//
//   ...more argument fragments...
//
//   data: {"id":"chatcmpl-...","choices":[
//           {"index":0,"delta":{},"finish_reason":"tool_calls"}]}
//
//   data: [DONE]
//
// Tool call accumulation:
//   OpenAI streams tool call arguments as partial strings in sequential
//   chunks, keyed by `tool_calls[i].index`.  We accumulate per-index
//   just like Anthropic, but the structure is different (function.name
//   and function.arguments instead of content_block with input_json_delta).
// ===========================================================================

use async_trait::async_trait;

use crate::auth::Auth;
use crate::error::{DysonError, Result};
use crate::llm::sse_parser::{BaseSseParser, SseJsonParser, ToolBufferContext};
use crate::llm::stream::{StopReason, StreamEvent};
use crate::llm::{CompletionConfig, LlmClient, ToolDefinition, concat_system_prompt};
use crate::message::{ContentBlock, Message, Role};

// ---------------------------------------------------------------------------
// OpenAiClient
// ---------------------------------------------------------------------------

/// OpenAI Chat Completions API client with SSE streaming.
///
/// Hardcoded to `https://api.openai.com`. For other OpenAI-compatible
/// endpoints, use [`super::openai_compat::OpenAiCompatClient`].
pub struct OpenAiClient {
    client: reqwest::Client,
    /// Authentication handler (applies `Authorization: Bearer` header).
    /// Zeroize is handled by the Auth implementation.
    auth: Box<dyn Auth>,
    base_url: String,
}

impl OpenAiClient {
    /// Create a new OpenAI client with an API key string.
    ///
    /// Convenience constructor — wraps the key in `BearerTokenAuth`.
    pub fn new(api_key: &str) -> Self {
        Self::with_auth(Box::new(crate::auth::BearerTokenAuth::new(
            api_key.to_string(),
        )))
    }

    /// Create a new OpenAI client with a custom `Auth` implementation.
    pub fn with_auth(auth: Box<dyn Auth>) -> Self {
        Self {
            client: crate::http::client().clone(),
            auth,
            base_url: "https://api.openai.com".to_string(),
        }
    }

    /// Create a client pointing at an arbitrary OpenAI-compatible endpoint.
    ///
    /// Used internally by [`super::openai_compat::OpenAiCompatClient`].
    pub(crate) fn with_base_url(auth: Box<dyn Auth>, base_url: &str) -> Self {
        Self {
            client: crate::http::client().clone(),
            auth,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// LlmClient implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmClient for OpenAiClient {
    async fn stream(
        &self,
        messages: &[Message],
        system: &str,
        system_suffix: &str,
        tools: &[ToolDefinition],
        config: &CompletionConfig,
    ) -> Result<crate::llm::StreamResponse> {
        // -- Build messages array --
        //
        // OpenAI puts the system prompt as the first message with role "system".
        // Then user/assistant messages follow.  Tool results use role "tool"
        // with a tool_call_id field (different from Anthropic's approach of
        // putting them in user messages).
        let mut messages_json: Vec<serde_json::Value> = Vec::new();

        // System message first.  Concatenate stable prefix and ephemeral suffix
        // (OpenAI doesn't support prompt caching breakpoints, so one block is fine).
        let full_system = concat_system_prompt(system, system_suffix);
        messages_json.push(serde_json::json!({
            "role": "system",
            "content": full_system,
        }));

        // Convert our internal messages to OpenAI format.
        for msg in messages {
            messages_json.push(message_to_openai(msg));
        }

        // -- Build tools array --
        //
        // OpenAI wraps tool definitions in a {"type":"function","function":{...}} envelope.
        let tools_json: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            })
            .collect();

        // -- Build request body --
        let mut body = serde_json::json!({
            "model": config.model,
            "max_tokens": config.max_tokens,
            "messages": messages_json,
            "stream": true,
        });

        if !tools_json.is_empty() {
            body["tools"] = serde_json::Value::Array(tools_json);
        }

        if let Some(temp) = config.temperature {
            body["temperature"] = serde_json::json!(temp);
        }

        // -- Send request --
        let url = format!("{}/v1/chat/completions", self.base_url);

        tracing::debug!(
            model = config.model,
            message_count = messages.len(),
            "sending OpenAI streaming request"
        );

        let req = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body);

        let response = self.auth.apply_to_request(req).await?.send().await?;

        if !response.status().is_success() {
            return Err(crate::llm::map_http_error(response, "OpenAI").await);
        }

        Ok(crate::llm::build_stream_response(
            response,
            BaseSseParser::new(OpenAiJsonParser::new()),
        ))
    }
}

// ---------------------------------------------------------------------------
// Message serialization — internal format → OpenAI format.
// ---------------------------------------------------------------------------

/// Collect all `Text` blocks into a single string.
fn extract_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Document { extracted_text, .. } => Some(extracted_text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Convert an internal Message to OpenAI's JSON format.
///
/// ## Key differences from Anthropic:
///
/// - Tool results use role "tool" (not "user") with a "tool_call_id" field.
/// - Tool calls in assistant messages go in a "tool_calls" array (not content blocks).
/// - Text content is a simple string field (not a content array), unless there
///   are also tool calls (in which case content can be null).
fn message_to_openai(msg: &Message) -> serde_json::Value {
    let role_str = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };

    // Check if this is a tool result message (User role with ToolResult content).
    if let Some(ContentBlock::ToolResult {
        tool_use_id,
        content,
        ..
    }) = msg.content.first()
        && matches!(msg.role, Role::User)
    {
        return serde_json::json!({
            "role": "tool",
            "tool_call_id": tool_use_id,
            "content": content,
        });
    }

    // Check if this is an assistant message with tool calls.
    let has_tool_uses = msg
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { .. }));

    if has_tool_uses {
        // Extract text content (if any) and tool calls separately.
        let text = extract_text(&msg.content);

        let tool_calls: Vec<serde_json::Value> = msg
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => Some(serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": input.to_string(),
                    }
                })),
                _ => None,
            })
            .collect();

        let content = if text.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!(text)
        };

        return serde_json::json!({
            "role": "assistant",
            "content": content,
            "tool_calls": tool_calls,
        });
    }

    // Check if this message contains any image or document blocks.
    let has_multimodal = msg
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::Image { .. } | ContentBlock::Document { .. }));

    if has_multimodal {
        // Multimodal format: content is an array of typed blocks.
        let content_array: Vec<serde_json::Value> = msg
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(serde_json::json!({
                    "type": "text",
                    "text": text,
                })),
                ContentBlock::Image { data, media_type } => Some(serde_json::json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{media_type};base64,{data}"),
                    }
                })),
                ContentBlock::Document { data, .. } => Some(serde_json::json!({
                    "type": "file",
                    "file": {
                        "filename": "document.pdf",
                        "file_data": format!("data:application/pdf;base64,{data}"),
                    }
                })),
                _ => None,
            })
            .collect();

        return serde_json::json!({
            "role": role_str,
            "content": content_array,
        });
    }

    // Simple text message.
    let text = extract_text(&msg.content);

    serde_json::json!({
        "role": role_str,
        "content": text,
    })
}

// ---------------------------------------------------------------------------
// OpenAI SSE Parser
// ---------------------------------------------------------------------------

/// OpenAI-specific SSE JSON parser.
///
/// Handles OpenAI's choices[0].delta event model.
/// Used with `BaseSseParser<OpenAiJsonParser>`.
struct OpenAiJsonParser {
    /// Whether we've already emitted a MessageComplete event.
    /// Guards against duplicate finish_reason chunks from providers
    /// like OpenRouter that may send usage data in a separate chunk.
    completed: bool,
}

impl OpenAiJsonParser {
    fn new() -> Self {
        Self { completed: false }
    }
}

impl SseJsonParser for OpenAiJsonParser {
    fn parse_json(
        &mut self,
        json: &serde_json::Value,
        ctx: &mut ToolBufferContext,
    ) -> Vec<Result<StreamEvent>> {
        let mut events = Vec::new();

        let Some(choices) = json["choices"].as_array() else {
            return events;
        };

        for choice in choices {
            let delta = &choice["delta"];

            // -- Thinking / reasoning content --
            if let Some(thinking) = delta["reasoning_content"].as_str()
                && !thinking.is_empty()
            {
                events.push(Ok(StreamEvent::ThinkingDelta(thinking.to_string())));
            }

            // -- Text content --
            if let Some(text) = delta["content"].as_str()
                && !text.is_empty()
            {
                events.push(Ok(StreamEvent::TextDelta(text.to_string())));
            }

            // -- Tool calls --
            if let Some(tool_calls) = delta["tool_calls"].as_array() {
                for tc in tool_calls {
                    let index = tc["index"].as_u64().unwrap_or(0) as usize;

                    if let Some(id) = tc["id"].as_str() {
                        let name = tc["function"]["name"].as_str().unwrap_or("").to_string();

                        if let Some(err_event) =
                            ctx.start_tool(index, id.to_string(), name.clone())
                        {
                            events.push(Err(DysonError::Llm(format!("{err_event:?}"))));
                            return events;
                        }

                        events.push(Ok(StreamEvent::ToolUseStart {
                            id: id.to_string(),
                            name,
                        }));
                    }

                    if let Some(args) = tc["function"]["arguments"].as_str() {
                        if let Some(err_event) = ctx.append_tool_json(index, args) {
                            events.push(Ok(err_event));
                        } else {
                            events
                                .push(Ok(StreamEvent::ToolUseInputDelta(args.to_string())));
                        }
                    }
                }
            }

            // -- Finish reason --
            if let Some(reason) = choice["finish_reason"].as_str()
                && !self.completed
            {
                self.completed = true;

                let output_tokens = json["usage"]["completion_tokens"]
                    .as_u64()
                    .map(|n| n as usize);

                match reason {
                    "stop" => {
                        events.push(Ok(StreamEvent::MessageComplete {
                            stop_reason: StopReason::EndTurn,
                            output_tokens,
                        }));
                    }
                    "tool_calls" => {
                        events.extend(ctx.drain_all());
                        events.push(Ok(StreamEvent::MessageComplete {
                            stop_reason: StopReason::ToolUse,
                            output_tokens,
                        }));
                    }
                    "length" => {
                        events.push(Ok(StreamEvent::MessageComplete {
                            stop_reason: StopReason::MaxTokens,
                            output_tokens,
                        }));
                    }
                    _ => {
                        tracing::warn!(finish_reason = reason, "unknown OpenAI finish_reason");
                        events.push(Ok(StreamEvent::MessageComplete {
                            stop_reason: StopReason::EndTurn,
                            output_tokens,
                        }));
                    }
                }
            }
        }

        events
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{SseStreamParser, MAX_ACTIVE_TOOL_BUFFERS};

    fn parse_sse(lines: &str) -> Vec<Result<StreamEvent>> {
        let mut parser = BaseSseParser::new(OpenAiJsonParser::new());
        parser.feed(lines.as_bytes())
    }

    #[test]
    fn parse_text_delta() {
        let events = parse_sse(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        );
        assert_eq!(events.len(), 1);
        match events[0].as_ref().unwrap() {
            StreamEvent::TextDelta(text) => assert_eq!(text, "Hello"),
            other => panic!("expected TextDelta, got: {other:?}"),
        }
    }

    #[test]
    fn parse_tool_call_lifecycle() {
        let sse = "\
            data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n\
            data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"com\"}}]},\"finish_reason\":null}]}\n\n\
            data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"mand\\\":\\\"ls\\\"}\"}}]},\"finish_reason\":null}]}\n\n\
            data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
            data: [DONE]\n\n";

        let events = parse_sse(sse);

        // ToolUseStart, InputDelta, InputDelta, InputDelta, ToolUseComplete, MessageComplete
        let tool_starts: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.as_ref().unwrap(), StreamEvent::ToolUseStart { .. }))
            .collect();
        assert_eq!(tool_starts.len(), 1);

        let completes: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.as_ref().unwrap(), StreamEvent::ToolUseComplete { .. }))
            .collect();
        assert_eq!(completes.len(), 1);

        match completes[0].as_ref().unwrap() {
            StreamEvent::ToolUseComplete { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "ls");
            }
            other => panic!("expected ToolUseComplete, got: {other:?}"),
        }
    }

    #[test]
    fn parse_stop_finish_reason() {
        let events = parse_sse(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        );
        match events.last().unwrap().as_ref().unwrap() {
            StreamEvent::MessageComplete { stop_reason, .. } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
            }
            other => panic!("expected MessageComplete, got: {other:?}"),
        }
    }

    #[test]
    fn parse_reasoning_content_as_thinking() {
        // OpenAI o-series and DeepSeek models emit reasoning_content in deltas.
        // These should become ThinkingDelta events, not TextDelta.
        let events = parse_sse(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"Let me think...\"},\"finish_reason\":null}]}\n\n\
             data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"The answer.\"},\"finish_reason\":null}]}\n\n\
             data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        );

        let thinking: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.as_ref().unwrap(), StreamEvent::ThinkingDelta(_)))
            .collect();
        assert_eq!(thinking.len(), 1);
        match thinking[0].as_ref().unwrap() {
            StreamEvent::ThinkingDelta(t) => assert_eq!(t, "Let me think..."),
            other => panic!("expected ThinkingDelta, got: {other:?}"),
        }

        let text: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.as_ref().unwrap(), StreamEvent::TextDelta(_)))
            .collect();
        assert_eq!(text.len(), 1);
        match text[0].as_ref().unwrap() {
            StreamEvent::TextDelta(t) => assert_eq!(t, "The answer."),
            other => panic!("expected TextDelta, got: {other:?}"),
        }
    }

    #[test]
    fn message_to_openai_user() {
        let msg = Message::user("hello");
        let json = message_to_openai(&msg);
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn message_to_openai_tool_result() {
        let msg = Message::tool_result("call_1", "output", false);
        let json = message_to_openai(&msg);
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_1");
        assert_eq!(json["content"], "output");
    }

    #[test]
    fn message_to_openai_assistant_with_tool_calls() {
        let msg = Message::assistant(vec![
            ContentBlock::Text {
                text: "Let me check.".into(),
            },
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
        ]);
        let json = message_to_openai(&msg);
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["tool_calls"][0]["id"], "call_1");
        assert_eq!(json["tool_calls"][0]["function"]["name"], "bash");
    }

    #[test]
    fn duplicate_finish_reason_emits_single_message_complete() {
        // OpenRouter and some providers send finish_reason in one chunk,
        // then a usage-only chunk that also includes finish_reason.
        // We must only emit one MessageComplete.
        let mut parser = BaseSseParser::new(OpenAiJsonParser::new());

        // First chunk: text + finish.
        let events1 = parser.feed(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n\
              data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
        );
        let complete_count = events1
            .iter()
            .filter(|e| matches!(e, Ok(StreamEvent::MessageComplete { .. })))
            .count();
        assert_eq!(
            complete_count, 1,
            "first batch should have exactly one MessageComplete"
        );

        // Second chunk: usage data with finish_reason again.
        let events2 = parser.feed(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"completion_tokens\":5}}\n\n\
              data: [DONE]\n\n"
        );
        let complete_count2 = events2
            .iter()
            .filter(|e| matches!(e, Ok(StreamEvent::MessageComplete { .. })))
            .count();
        assert_eq!(
            complete_count2, 0,
            "duplicate finish_reason should not emit another MessageComplete"
        );
    }

    #[test]
    fn line_buffer_rejects_oversized_input() {
        let mut parser = BaseSseParser::new(OpenAiJsonParser::new());
        let chunk = vec![b'x'; 10 * 1024 * 1024 + 1];
        let events = parser.feed(&chunk);
        assert_eq!(events.len(), 1);
        assert!(events[0].is_err());
        let err_msg = format!("{}", events[0].as_ref().unwrap_err());
        assert!(
            err_msg.contains("10 MB"),
            "error should mention size limit, got: {err_msg}"
        );
    }

    #[test]
    fn too_many_tool_buffers_emits_error() {
        let mut parser = BaseSseParser::new(OpenAiJsonParser::new());
        // Insert MAX_ACTIVE_TOOL_BUFFERS + 1 tool calls.
        for i in 0..=MAX_ACTIVE_TOOL_BUFFERS {
            let sse = format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":{i},\"id\":\"call_{i}\",\"type\":\"function\",\"function\":{{\"name\":\"bash\",\"arguments\":\"\"}}}}]}},\"finish_reason\":null}}]}}\n\n"
            );
            let events = parser.feed(sse.as_bytes());
            if i == MAX_ACTIVE_TOOL_BUFFERS {
                let has_error = events.iter().any(|e| e.is_err());
                assert!(has_error, "expected error on tool buffer #{i}");
            }
        }
    }

    #[test]
    fn tool_json_buffer_rejects_oversized_input() {
        let mut parser = BaseSseParser::new(OpenAiJsonParser::new());

        // Start a tool call.
        let start = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n";
        parser.feed(start.as_bytes());

        // Feed chunks that exceed 10 MB.
        let big_chunk = "x".repeat(1024 * 1024);
        for i in 0..11 {
            let delta = format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":\"{big_chunk}\"}}}}]}},\"finish_reason\":null}}]}}\n\n"
            );
            let events = parser.feed(delta.as_bytes());
            if i >= 10 {
                let has_overflow = events
                    .iter()
                    .any(|e| matches!(e, Ok(StreamEvent::TextDelta(t)) if t.contains("10 MB")));
                assert!(has_overflow, "expected tool buffer overflow on chunk {i}");
            }
        }
    }
}
