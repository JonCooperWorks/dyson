// ===========================================================================
// Anthropic client — streaming SSE implementation of the Messages API.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements `LlmClient` for the Anthropic Messages API.  This is the
//   most complex file in Phase 1 because it handles:
//   - Building the HTTP request (messages, tools, system prompt)
//   - Parsing Server-Sent Events (SSE) from the response stream
//   - Accumulating partial tool_use JSON from delta events
//   - Emitting well-typed `StreamEvent`s for the agent's stream handler
//
// How Anthropic streaming works:
//
//   POST /v1/messages  (with stream: true)
//     ↓
//   Server sends SSE events (text/event-stream):
//     event: message_start
//     data: {"type":"message_start","message":{"id":"msg_...","role":"assistant",...}}
//
//     event: content_block_start
//     data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}
//
//     event: content_block_delta
//     data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}
//
//     event: content_block_stop
//     data: {"type":"content_block_stop","index":0}
//
//     event: message_delta
//     data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},...}
//
//     event: message_stop
//     data: {"type":"message_stop"}
//
// For tool calls, the flow is:
//     content_block_start  → type: "tool_use", id, name
//     content_block_delta  → type: "input_json_delta", partial_json: "{"
//     content_block_delta  → type: "input_json_delta", partial_json: "\"command\""
//     ...more delta events with JSON fragments...
//     content_block_stop   → we parse the accumulated JSON → ToolUseComplete
//
// SSE parsing:
//   SSE (Server-Sent Events) is a simple text protocol:
//   - Lines starting with "event:" set the event type (we mostly ignore this)
//   - Lines starting with "data:" contain the JSON payload
//   - Empty lines delimit events
//   - Some providers send "data: [DONE]" to signal stream end; Anthropic
//     uses a `message_stop` event instead, so we don't rely on [DONE]
//
//   The tricky part: the byte stream from reqwest can split ANYWHERE — in the
//   middle of a line, in the middle of a UTF-8 character, or across event
//   boundaries.  We buffer bytes and split on newlines to handle this.
//
// Tool use accumulation:
//   The Anthropic API streams tool input as partial JSON strings.  We need
//   to concatenate all the `input_json_delta` fragments for a given content
//   block index, then parse the final JSON when `content_block_stop` arrives.
//   We use a HashMap<usize, ToolCallBuffer> to track active tool calls by
//   their content block index.
// ===========================================================================

use async_trait::async_trait;

use crate::auth::Auth;
use crate::error::{DysonError, Result};
use crate::llm::sse_parser::{BaseSseParser, SseJsonParser, ToolBufferContext};
use crate::llm::stream::{StopReason, StreamEvent};
use crate::llm::{CompletionConfig, LlmClient, ToolDefinition};
use crate::message::{ContentBlock, Message, Role};

// ---------------------------------------------------------------------------
// Anthropic message serialization
// ---------------------------------------------------------------------------

/// Serialize a `Message` to the JSON shape expected by the Anthropic Messages API.
///
/// Each LLM provider is responsible for converting from the internal `Message`
/// type to its own wire format.  This is the Anthropic version — see
/// `message_to_openai()` in `openai.rs` for the OpenAI equivalent.
///
/// Rather than annotating everything with serde renames and custom serializers,
/// we build the JSON value directly.  This is explicit, easy to debug, and
/// trivial to adapt when adding new providers.
fn message_to_anthropic(msg: &Message) -> serde_json::Value {
    let role_str = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };

    let content: Vec<serde_json::Value> = msg
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(serde_json::json!({
                "type": "text",
                "text": text,
            })),
            ContentBlock::ToolUse { id, name, input } => Some(serde_json::json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            })),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some(serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            })),
            ContentBlock::Thinking { thinking } => Some(serde_json::json!({
                "type": "thinking",
                "thinking": thinking,
            })),
            ContentBlock::Image { data, media_type } => Some(serde_json::json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": media_type,
                    "data": data,
                }
            })),
            ContentBlock::Document { data, .. } => Some(serde_json::json!({
                "type": "document",
                "source": {
                    "type": "base64",
                    "media_type": "application/pdf",
                    "data": data,
                }
            })),
            // Artefact is a UI side-channel; the LLM never sees it.
            ContentBlock::Artefact { .. } => None,
        })
        .collect();

    serde_json::json!({
        "role": role_str,
        "content": content,
    })
}

/// Anthropic API version header.  Pinned to a specific version to avoid
/// unexpected behaviour if the API evolves.  Bump this when adopting new
/// API features.
///
/// 2024-11-05 is required for prompt caching (GA) — allows `cache_control`
/// on system prompt blocks, tool definitions, and message content blocks.
const ANTHROPIC_API_VERSION: &str = "2024-11-05";

// ---------------------------------------------------------------------------
// AnthropicClient
// ---------------------------------------------------------------------------

/// Anthropic Messages API client with SSE streaming.
///
/// Sends requests to `https://api.anthropic.com/v1/messages` (or a custom
/// base URL) and parses the SSE response into a `Stream<StreamEvent>`.
pub struct AnthropicClient {
    /// Reusable HTTP client (connection pooling, TLS setup, etc.).
    client: reqwest::Client,

    /// Authentication handler (applies `x-api-key` header).
    /// Zeroize is handled by the Auth implementation.
    auth: Box<dyn Auth>,

    base_url: String,
}

impl AnthropicClient {
    /// Create a new Anthropic client with an API key string.
    ///
    /// Convenience constructor — wraps the key in `ApiKeyAuth::anthropic()`.
    pub fn new(api_key: &str) -> Self {
        Self::with_auth(Box::new(crate::auth::ApiKeyAuth::anthropic(
            api_key.to_string(),
        )))
    }

    /// Create a new Anthropic client with a custom `Auth` implementation.
    ///
    /// ```ignore
    /// let auth = ApiKeyAuth::anthropic(key);
    /// let client = AnthropicClient::with_auth(Box::new(auth));
    /// ```
    pub fn with_auth(auth: Box<dyn Auth>) -> Self {
        Self {
            client: crate::http::client().clone(),
            auth,
            base_url: "https://api.anthropic.com".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// LlmClient implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmClient for AnthropicClient {
    /// Stream a completion from the Anthropic Messages API.
    ///
    /// ## Request building
    ///
    /// We build the request body as a `serde_json::Value` rather than a
    /// typed struct because the Anthropic API shape is complex and version-
    /// dependent.  Manual JSON construction is explicit and easy to debug.
    ///
    /// ## Response handling
    ///
    /// The response is an SSE (text/event-stream) byte stream.  We use
    /// `async_stream::stream!` to transform it into `StreamEvent`s.
    /// Inside the stream, an `SseParser` handles line buffering and
    /// tool_use accumulation.
    async fn stream(
        &self,
        messages: &[Message],
        system: &str,
        system_suffix: &str,
        tools: &[ToolDefinition],
        config: &CompletionConfig,
    ) -> Result<crate::llm::StreamResponse> {
        // -- Build the request body --
        //
        // Prompt caching strategy (up to 4 breakpoints allowed):
        //
        //   1. System prompt block — marked with cache_control so the large
        //      system prompt (identity files, tool descriptions, etc.) is
        //      cached across turns within a session.
        //
        //   2. System suffix block — ephemeral per-turn context (timestamps,
        //      skill fragments) that changes every turn.  NOT cached, so it
        //      doesn't bust the KV cache for the stable prefix above.
        //
        //   3. Last tool definition — tools are stable within a session, so
        //      caching the full tool array avoids re-processing on every turn.
        //
        //   4. Penultimate user message — the conversation history grows
        //      monotonically.  Caching up to a recent message means only the
        //      latest turn needs processing.  We pick the second-to-last
        //      user-role message so the cache covers the stable prefix.
        //
        // The API requires `"system"` as an array of content blocks (not a
        // plain string) when using `cache_control`.

        let mut system_blocks_vec = vec![
            serde_json::json!({
                "type": "text",
                "text": system,
                "cache_control": { "type": "ephemeral" }
            }),
        ];

        // Append the ephemeral suffix as a separate block (no cache_control)
        // so the stable prefix remains cacheable across turns.
        if !system_suffix.is_empty() {
            system_blocks_vec.push(serde_json::json!({
                "type": "text",
                "text": system_suffix,
            }));
        }

        let system_blocks = serde_json::Value::Array(system_blocks_vec);

        let mut messages_json: Vec<serde_json::Value> =
            messages.iter().map(message_to_anthropic).collect();

        // Add cache breakpoint on the second-to-last message's last content
        // block.  This caches the stable conversation prefix so only the
        // newest turn needs re-processing.
        if messages_json.len() >= 2 {
            let cache_idx = messages_json.len() - 2;
            if let Some(content) = messages_json[cache_idx]["content"].as_array_mut()
                && let Some(last_block) = content.last_mut()
            {
                last_block["cache_control"] = serde_json::json!({ "type": "ephemeral" });
            }
        }

        let mut tools_json: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();

        // Inject provider-native tool entries (e.g., Anthropic advisor).
        for entry in &config.api_tool_injections {
            tools_json.push(entry.clone());
        }

        // Mark the last tool with cache_control so the entire tool set is cached.
        if let Some(last_tool) = tools_json.last_mut() {
            last_tool["cache_control"] = serde_json::json!({ "type": "ephemeral" });
        }

        let mut body = serde_json::json!({
            "model": config.model,
            "max_tokens": config.max_tokens,
            "system": system_blocks,
            "messages": messages_json,
            "stream": true,
        });

        // Only include tools if we have any (the API rejects an empty tools array).
        if !tools_json.is_empty() {
            body["tools"] = serde_json::Value::Array(tools_json);
        }

        if let Some(temp) = config.temperature {
            body["temperature"] = serde_json::json!(temp);
        }

        // -- Send the request --
        let url = format!("{}/v1/messages", self.base_url);

        tracing::debug!(
            model = config.model,
            max_tokens = config.max_tokens,
            message_count = messages.len(),
            tool_count = tools.len(),
            "sending Anthropic streaming request"
        );

        let req = self
            .client
            .post(&url)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("content-type", "application/json")
            .json(&body);

        let response = self.auth.apply_to_request(req).await?.send().await?;

        // -- Check for HTTP errors --
        //
        // Non-2xx responses mean the API rejected the request (bad auth,
        // rate limit, malformed request, etc.).  We read the body for
        // the error message rather than just returning the status code.
        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "failed to read error body".into());

            // Log the full body for debugging, but only surface a summary
            // to the agent to avoid leaking internal API details.
            tracing::error!(status = %status, body = %body, "Anthropic API error");

            return Err(match status.as_u16() {
                401 => DysonError::Llm("Anthropic API error: authentication failed (check API key)".into()),
                429 => DysonError::LlmRateLimit("Anthropic API rate limited — try again shortly".into()),
                502 | 503 => DysonError::LlmOverloaded(format!("Anthropic API returned HTTP {status}")),
                529 => DysonError::LlmOverloaded("Anthropic API overloaded — try again shortly".into()),
                _ => DysonError::Llm(format!("Anthropic API error: HTTP {status}")),
            });
        }

        Ok(crate::llm::build_stream_response(
            response,
            BaseSseParser::new(AnthropicJsonParser),
        ))
    }
}

// ---------------------------------------------------------------------------
// SSE Parser — converts raw bytes into StreamEvents.
// ---------------------------------------------------------------------------

/// Anthropic-specific SSE JSON parser.
///
/// Handles Anthropic's content_block_start/delta/stop event model.
/// Used with `BaseSseParser<AnthropicJsonParser>`.
struct AnthropicJsonParser;

impl SseJsonParser for AnthropicJsonParser {
    fn parse_json(
        &mut self,
        json: &serde_json::Value,
        ctx: &mut ToolBufferContext,
    ) -> Vec<Result<StreamEvent>> {
        let mut events = Vec::new();

        let Some(event_type) = json["type"].as_str() else {
            return events;
        };

        match event_type {
            "content_block_start" => {
                let Some(index) = json["index"].as_u64().map(|n| n as usize) else {
                    return events;
                };
                let block = &json["content_block"];
                let Some(block_type) = block["type"].as_str() else {
                    return events;
                };

                if block_type == "tool_use" {
                    let id = block["id"].as_str().unwrap_or("").to_string();
                    let name = block["name"].as_str().unwrap_or("").to_string();

                    if let Some(err_event) = ctx.start_tool(index, id.clone(), name.clone()) {
                        events.push(Ok(err_event));
                        return events;
                    }

                    events.push(Ok(StreamEvent::ToolUseStart { id, name }));
                } else if block_type == "thinking" {
                    ctx.thinking_blocks.insert(index);
                }
            }

            "content_block_delta" => {
                let Some(index) = json["index"].as_u64().map(|n| n as usize) else {
                    return events;
                };
                let delta = &json["delta"];
                let Some(delta_type) = delta["type"].as_str() else {
                    return events;
                };

                match delta_type {
                    "thinking_delta" => {
                        if let Some(text) = delta["thinking"].as_str() {
                            events.push(Ok(StreamEvent::ThinkingDelta(text.to_string())));
                        }
                    }
                    "text_delta" => {
                        if let Some(text) = delta["text"].as_str() {
                            if ctx.thinking_blocks.contains(&index) {
                                events.push(Ok(StreamEvent::ThinkingDelta(text.to_string())));
                            } else {
                                events.push(Ok(StreamEvent::TextDelta(text.to_string())));
                            }
                        }
                    }
                    "input_json_delta" => {
                        if let Some(partial) = delta["partial_json"].as_str() {
                            if let Some(err_event) = ctx.append_tool_json(index, partial) {
                                events.push(Ok(err_event));
                                return events;
                            }
                            events.push(Ok(StreamEvent::ToolUseInputDelta(partial.to_string())));
                        }
                    }
                    _ => {}
                }
            }

            "content_block_stop" => {
                let Some(index) = json["index"].as_u64().map(|n| n as usize) else {
                    return events;
                };
                if let Some(result) = ctx.finalize_tool(index) {
                    match result {
                        Ok(event) => events.push(Ok(event)),
                        Err(e) => {
                            tracing::error!(error = %e, "tool call finalization failed");
                        }
                    }
                }
            }

            "message_delta" => {
                if let Some(stop_str) = json["delta"]["stop_reason"].as_str() {
                    let stop_reason = match stop_str {
                        "end_turn" => StopReason::EndTurn,
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::MaxTokens,
                        other => {
                            tracing::warn!(stop_reason = other, "unknown stop reason");
                            StopReason::EndTurn
                        }
                    };
                    let output_tokens =
                        json["usage"]["output_tokens"].as_u64().map(|n| n as usize);
                    events.push(Ok(StreamEvent::MessageComplete {
                        stop_reason,
                        output_tokens,
                    }));
                }
            }

            "error" => {
                let message = json["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown error")
                    .to_string();
                let error_type = json["error"]["type"].as_str().unwrap_or("");
                let err = match error_type {
                    "overloaded_error" => DysonError::LlmOverloaded(message),
                    "rate_limit_error" => DysonError::LlmRateLimit(message),
                    _ => DysonError::Llm(message),
                };
                events.push(Ok(StreamEvent::Error(err)));
            }

            _ => {}
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

    /// Helper: feed SSE lines through the parser and collect events.
    fn parse_sse(lines: &str) -> Vec<Result<StreamEvent>> {
        let mut parser = BaseSseParser::new(AnthropicJsonParser);
        parser.feed(lines.as_bytes())
    }

    #[test]
    fn parse_text_delta() {
        let events = parse_sse(
            "event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        );
        assert_eq!(events.len(), 1);
        match events[0].as_ref().unwrap() {
            StreamEvent::TextDelta(text) => assert_eq!(text, "Hello"),
            other => panic!("expected TextDelta, got: {other:?}"),
        }
    }

    #[test]
    fn parse_tool_use_lifecycle() {
        // Simulate a complete tool_use: start → deltas → stop.
        let sse = "\
            event: content_block_start\n\
            data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"bash\"}}\n\n\
            event: content_block_delta\n\
            data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"com\"}}\n\n\
            event: content_block_delta\n\
            data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"mand\\\":\\\"ls\\\"}\"}}\n\n\
            event: content_block_stop\n\
            data: {\"type\":\"content_block_stop\",\"index\":1}\n\n";

        let events = parse_sse(sse);

        // Should get: ToolUseStart, InputDelta, InputDelta, ToolUseComplete
        assert_eq!(events.len(), 4);

        match events[0].as_ref().unwrap() {
            StreamEvent::ToolUseStart { id, name } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "bash");
            }
            other => panic!("expected ToolUseStart, got: {other:?}"),
        }

        match events[3].as_ref().unwrap() {
            StreamEvent::ToolUseComplete { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "ls");
            }
            other => panic!("expected ToolUseComplete, got: {other:?}"),
        }
    }

    #[test]
    fn parse_message_complete() {
        let events = parse_sse(
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\n",
        );
        assert_eq!(events.len(), 1);
        match events[0].as_ref().unwrap() {
            StreamEvent::MessageComplete { stop_reason, .. } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
            }
            other => panic!("expected MessageComplete, got: {other:?}"),
        }
    }

    #[test]
    fn parse_tool_use_stop_reason() {
        let events = parse_sse(
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
        );
        match events[0].as_ref().unwrap() {
            StreamEvent::MessageComplete { stop_reason, .. } => {
                assert_eq!(*stop_reason, StopReason::ToolUse);
            }
            other => panic!("expected MessageComplete, got: {other:?}"),
        }
    }

    #[test]
    fn parse_error_event() {
        let events = parse_sse(
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        );
        assert_eq!(events.len(), 1);
        match events[0].as_ref().unwrap() {
            StreamEvent::Error(DysonError::LlmOverloaded(msg)) => assert_eq!(msg, "Overloaded"),
            other => panic!("expected Error, got: {other:?}"),
        }
    }

    #[test]
    fn handles_partial_lines() {
        // Feed bytes in two chunks that split in the middle of a line.
        let mut parser = BaseSseParser::new(AnthropicJsonParser);

        // First chunk: incomplete line.
        let events1 = parser.feed(b"data: {\"type\":\"content_block_del");
        assert!(events1.is_empty(), "no complete line yet");

        // Second chunk: rest of the line + newline.
        let events2 = parser
            .feed(b"ta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n");
        assert_eq!(events2.len(), 1);
        match events2[0].as_ref().unwrap() {
            StreamEvent::TextDelta(text) => assert_eq!(text, "Hi"),
            other => panic!("expected TextDelta, got: {other:?}"),
        }
    }

    #[test]
    fn parse_thinking_block_as_thinking_delta() {
        // Anthropic extended thinking: a content_block_start with type "thinking"
        // followed by thinking_delta events.
        let sse = "\
            event: content_block_start\n\
            data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n\
            event: content_block_delta\n\
            data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me reason...\"}}\n\n\
            event: content_block_stop\n\
            data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
            event: content_block_start\n\
            data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
            event: content_block_delta\n\
            data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"The answer.\"}}\n\n";

        let events = parse_sse(sse);

        let thinking: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.as_ref().unwrap(), StreamEvent::ThinkingDelta(_)))
            .collect();
        assert_eq!(thinking.len(), 1);
        match thinking[0].as_ref().unwrap() {
            StreamEvent::ThinkingDelta(t) => assert_eq!(t, "Let me reason..."),
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
    fn ignores_ping_and_message_start() {
        let events = parse_sse(
            "event: ping\n\
             data: {\"type\":\"ping\"}\n\n\
             event: message_start\n\
             data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_123\",\"role\":\"assistant\"}}\n\n",
        );
        assert!(events.is_empty());
    }

    // -----------------------------------------------------------------------
    // message_to_anthropic tests
    // -----------------------------------------------------------------------

    #[test]
    fn anthropic_serialization_user() {
        let msg = Message::user("hi");
        let val = message_to_anthropic(&msg);
        assert_eq!(val["role"], "user");
        assert_eq!(val["content"][0]["type"], "text");
        assert_eq!(val["content"][0]["text"], "hi");
    }

    #[test]
    fn anthropic_serialization_tool_use() {
        let msg = Message::assistant(vec![ContentBlock::ToolUse {
            id: "id_1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "echo test"}),
        }]);
        let val = message_to_anthropic(&msg);
        assert_eq!(val["role"], "assistant");
        assert_eq!(val["content"][0]["type"], "tool_use");
        assert_eq!(val["content"][0]["id"], "id_1");
        assert_eq!(val["content"][0]["name"], "bash");
        assert_eq!(val["content"][0]["input"]["command"], "echo test");
    }

    #[test]
    fn anthropic_serialization_tool_result() {
        let msg = Message::tool_result("id_1", "output here", true);
        let val = message_to_anthropic(&msg);
        assert_eq!(val["role"], "user");
        assert_eq!(val["content"][0]["type"], "tool_result");
        assert_eq!(val["content"][0]["tool_use_id"], "id_1");
        assert_eq!(val["content"][0]["content"], "output here");
        assert_eq!(val["content"][0]["is_error"], true);
    }

    // -----------------------------------------------------------------------
    // Buffer overflow protection tests
    // -----------------------------------------------------------------------

    #[test]
    fn line_buffer_rejects_oversized_input() {
        let mut parser = BaseSseParser::new(AnthropicJsonParser);

        // Feed just over 10 MB without any newlines — should trigger the cap.
        let chunk = vec![b'x'; 10 * 1024 * 1024 + 1];
        let events = parser.feed(&chunk);

        assert_eq!(events.len(), 1);
        assert!(events[0].is_err());
        let err_msg = format!("{}", events[0].as_ref().unwrap_err());
        assert!(
            err_msg.contains("10 MB"),
            "error should mention the size limit, got: {err_msg}"
        );
    }

    #[test]
    fn line_buffer_accepts_large_but_valid_input() {
        let mut parser = BaseSseParser::new(AnthropicJsonParser);

        // Feed a large but valid SSE line (under the cap) with a newline.
        let text = "x".repeat(1024);
        let line = format!(
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
        );
        let events = parser.feed(line.as_bytes());

        // Should parse successfully — one TextDelta event.
        assert_eq!(events.len(), 1);
        assert!(events[0].is_ok());
    }

    #[test]
    fn too_many_tool_buffers_emits_error() {
        let mut parser = BaseSseParser::new(AnthropicJsonParser);

        // Start MAX_ACTIVE_TOOL_BUFFERS + 1 tool_use blocks without closing any.
        for i in 0..=MAX_ACTIVE_TOOL_BUFFERS {
            let start = format!(
                "event: content_block_start\n\
                 data: {{\"type\":\"content_block_start\",\"index\":{i},\"content_block\":{{\"type\":\"tool_use\",\"id\":\"call_{i}\",\"name\":\"bash\"}}}}\n\n"
            );
            let events = parser.feed(start.as_bytes());

            if i == MAX_ACTIVE_TOOL_BUFFERS {
                // The 101st tool should trigger an error.
                let has_error = events.iter().any(|e| {
                    matches!(e, Ok(StreamEvent::Error(DysonError::Llm(msg))) if msg.contains("too many"))
                });
                assert!(has_error, "expected 'too many' error on tool buffer #{i}");
            }
        }
    }

    #[test]
    fn message_delta_includes_output_tokens() {
        let events = parse_sse(
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\n",
        );
        assert_eq!(events.len(), 1);
        match events[0].as_ref().unwrap() {
            StreamEvent::MessageComplete {
                stop_reason,
                output_tokens,
            } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
                assert_eq!(*output_tokens, Some(42));
            }
            other => panic!("expected MessageComplete, got: {other:?}"),
        }
    }

    #[test]
    fn message_delta_without_usage_has_none_tokens() {
        let events = parse_sse(
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        );
        assert_eq!(events.len(), 1);
        match events[0].as_ref().unwrap() {
            StreamEvent::MessageComplete { output_tokens, .. } => {
                assert_eq!(*output_tokens, None);
            }
            other => panic!("expected MessageComplete, got: {other:?}"),
        }
    }

    #[test]
    fn tool_json_buffer_rejects_oversized_input() {
        let mut parser = BaseSseParser::new(AnthropicJsonParser);

        // Start a tool_use block.
        let start = "event: content_block_start\n\
                     data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"bash\"}}\n\n";
        parser.feed(start.as_bytes());

        // Feed many input_json_delta chunks to exceed 10 MB.
        let big_chunk = "x".repeat(1024 * 1024); // 1 MB per chunk
        for i in 0..11 {
            let delta = format!(
                "event: content_block_delta\n\
                 data: {{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{big_chunk}\"}}}}\n\n"
            );
            let events = parser.feed(delta.as_bytes());

            if i >= 10 {
                // After 10+ MB, should get an error event.
                let has_error = events
                    .iter()
                    .any(|e| matches!(e, Ok(StreamEvent::TextDelta(t)) if t.contains("10 MB")));
                assert!(
                    has_error,
                    "expected tool buffer overflow error on chunk {i}"
                );
            }
        }
    }
}
