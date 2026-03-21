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
//   - "data: [DONE]" signals stream end (Anthropic uses message_stop instead)
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

use std::collections::HashMap;

use async_trait::async_trait;
use tokio_stream::StreamExt;

use crate::auth::Auth;
use crate::error::{DysonError, Result};
use crate::llm::stream::{StopReason, StreamEvent};
use crate::llm::{CompletionConfig, LlmClient, SseLineBuffer, ToolCallBuffer, ToolDefinition, finalize_tool_call, MAX_TOOL_JSON, MAX_ACTIVE_TOOL_BUFFERS};
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
        .map(|block| match block {
            ContentBlock::Text { text } => {
                serde_json::json!({
                    "type": "text",
                    "text": text,
                })
            }
            ContentBlock::ToolUse { id, name, input } => {
                serde_json::json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                })
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                    "is_error": is_error,
                })
            }
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
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

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

    /// Base URL for the API (default: "https://api.anthropic.com").
    ///
    /// Configurable for testing with mock servers or for proxied setups.
    base_url: String,
}

impl AnthropicClient {
    /// Create a new Anthropic client with an API key string.
    ///
    /// Convenience constructor — wraps the key in `ApiKeyAuth::anthropic()`.
    /// `base_url` is optional — pass `None` for the default Anthropic endpoint.
    pub fn new(api_key: &str, base_url: Option<&str>) -> Self {
        Self::with_auth(
            Box::new(crate::auth::ApiKeyAuth::anthropic(api_key.to_string())),
            base_url,
        )
    }

    /// Create a new Anthropic client with a custom `Auth` implementation.
    ///
    /// Use this for composable auth (e.g., adding audit logging):
    /// ```ignore
    /// let auth = TracingAuth::new(
    ///     Box::new(ApiKeyAuth::anthropic(key)),
    ///     "anthropic",
    /// );
    /// let client = AnthropicClient::with_auth(Box::new(auth), None);
    /// ```
    pub fn with_auth(auth: Box<dyn Auth>, base_url: Option<&str>) -> Self {
        Self {
            client: reqwest::Client::new(),
            auth,
            base_url: base_url
                .unwrap_or("https://api.anthropic.com")
                .to_string(),
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
        tools: &[ToolDefinition],
        config: &CompletionConfig,
    ) -> Result<crate::llm::StreamResponse> {
        // -- Build the request body --
        let messages_json: Vec<serde_json::Value> =
            messages.iter().map(message_to_anthropic).collect();

        let tools_json: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": config.model,
            "max_tokens": config.max_tokens,
            "system": system,
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

            let summary = match status.as_u16() {
                401 => "authentication failed (check API key)".to_string(),
                429 => "rate limited — try again shortly".to_string(),
                529 => "Anthropic API overloaded — try again shortly".to_string(),
                _ => format!("HTTP {status}"),
            };
            return Err(DysonError::Llm(format!(
                "Anthropic API error: {summary}"
            )));
        }

        // -- Transform the SSE byte stream into StreamEvents --
        //
        // `response.bytes_stream()` gives us a Stream<Result<Bytes>>.
        // We wrap it in an async_stream that buffers bytes, splits on
        // newlines, and parses SSE events into StreamEvents.
        let byte_stream = response.bytes_stream();

        let event_stream = async_stream::stream! {
            let mut parser = SseParser::new();

            tokio::pin!(byte_stream);

            while let Some(chunk_result) = byte_stream.next().await {
                match chunk_result {
                    Ok(bytes) => {
                        // Feed raw bytes into the SSE parser.
                        // It handles line splitting, JSON parsing, and
                        // tool_use accumulation internally.
                        let events = parser.feed(&bytes);
                        for event in events {
                            yield event;
                        }
                    }
                    Err(e) => {
                        yield Err(DysonError::Http(e));
                    }
                }
            }
        };

        Ok(crate::llm::StreamResponse {
            stream: Box::pin(event_stream),
            tool_mode: crate::llm::ToolMode::Execute,
            input_tokens: None,
        })
    }
}

// ---------------------------------------------------------------------------
// SSE Parser — converts raw bytes into StreamEvents.
// ---------------------------------------------------------------------------

/// Buffered parser for Anthropic's Server-Sent Events stream.
///
/// Handles three concerns:
/// 1. **Line buffering**: bytes can split anywhere; we buffer until we have
///    complete lines (delimited by `\n`).
/// 2. **SSE protocol**: extracts `data:` lines, ignores `event:` and comments.
/// 3. **Tool use accumulation**: tracks partial JSON for each tool_use content
///    block and emits `ToolUseComplete` when the block stops.
struct SseParser {
    /// Shared line buffer that handles SSE framing.
    line_buffer: SseLineBuffer,

    /// Active tool_use blocks being accumulated.
    ///
    /// Key: content block index (from the Anthropic API).
    /// Value: (tool_use_id, tool_name, accumulated_json_string).
    ///
    /// When `content_block_start` arrives with type "tool_use", we insert
    /// an entry.  Each `input_json_delta` appends to the JSON string.
    /// On `content_block_stop`, we parse the JSON and emit ToolUseComplete.
    tool_buffers: HashMap<usize, ToolCallBuffer>,

    /// Content block indices that are "thinking" blocks.
    ///
    /// Anthropic's extended thinking emits thinking content as regular
    /// content_block_delta events with type "thinking_delta".  We track
    /// which blocks are thinking so we can emit ThinkingDelta instead of
    /// TextDelta for their content.
    thinking_blocks: std::collections::HashSet<usize>,
}

impl SseParser {
    fn new() -> Self {
        Self {
            line_buffer: SseLineBuffer::new(),
            tool_buffers: HashMap::new(),
            thinking_blocks: std::collections::HashSet::new(),
        }
    }

    /// Feed raw bytes into the parser.
    ///
    /// Returns zero or more `StreamEvent`s.  Bytes are buffered internally
    /// until complete lines are available, so it's safe to call this with
    /// arbitrary chunk sizes (even mid-character for UTF-8).
    fn feed(&mut self, bytes: &[u8]) -> Vec<Result<StreamEvent>> {
        let mut events = Vec::new();

        // Use the shared line buffer for SSE framing.
        let payloads = match self.line_buffer.feed(bytes) {
            Ok(p) => p,
            Err(e) => {
                events.push(Err(e));
                return events;
            }
        };

        for data in payloads {
            // "[DONE]" is an OpenAI convention; Anthropic uses
            // "message_stop" events instead.  Handle both for safety.
            if data == "[DONE]" {
                continue;
            }

            // Parse the JSON payload and convert to StreamEvent(s).
            match serde_json::from_str::<serde_json::Value>(&data) {
                Ok(json) => {
                    if let Some(event) = self.parse_sse_json(&json) {
                        events.push(Ok(event));
                    }
                }
                Err(e) => {
                    tracing::warn!(data = data, error = %e, "failed to parse SSE JSON");
                }
            }
        }

        events
    }

    /// Parse a single SSE JSON payload into a StreamEvent.
    ///
    /// ## Anthropic event types
    ///
    /// | API "type" field       | StreamEvent                              |
    /// |------------------------|------------------------------------------|
    /// | message_start          | (ignored — we don't need the message ID) |
    /// | content_block_start    | ToolUseStart (if tool_use block)         |
    /// | content_block_delta    | TextDelta or ToolUseInputDelta           |
    /// | content_block_stop     | ToolUseComplete (if tool_use block)      |
    /// | message_delta          | MessageComplete                          |
    /// | message_stop           | (ignored — stream just ends)             |
    /// | ping                   | (ignored)                                |
    /// | error                  | Error                                    |
    fn parse_sse_json(&mut self, json: &serde_json::Value) -> Option<StreamEvent> {
        let event_type = json["type"].as_str()?;

        match event_type {
            // -- content_block_start --
            //
            // A new content block is beginning.  For text blocks, we don't
            // need to do anything (text arrives via deltas).  For tool_use
            // blocks, we start accumulating the input JSON.
            "content_block_start" => {
                let index = json["index"].as_u64()? as usize;
                let block = &json["content_block"];
                let block_type = block["type"].as_str()?;

                if block_type == "tool_use" {
                    let id = block["id"].as_str()?.to_string();
                    let name = block["name"].as_str()?.to_string();

                    // Guard against unbounded growth from a malformed stream
                    // that sends many content_block_start events without
                    // matching content_block_stop events.
                    if self.tool_buffers.len() >= MAX_ACTIVE_TOOL_BUFFERS {
                        return Some(StreamEvent::Error(DysonError::Llm(
                            format!(
                                "too many concurrent tool calls ({MAX_ACTIVE_TOOL_BUFFERS}) — aborting stream"
                            ),
                        )));
                    }

                    // Start accumulating JSON for this tool call.
                    self.tool_buffers.insert(
                        index,
                        ToolCallBuffer {
                            id: id.clone(),
                            name: name.clone(),
                            json: String::new(),
                        },
                    );

                    return Some(StreamEvent::ToolUseStart { id, name });
                }

                // Track thinking blocks so we can route their deltas correctly.
                if block_type == "thinking" {
                    self.thinking_blocks.insert(index);
                }

                None
            }

            // -- content_block_delta --
            //
            // A fragment of content for an existing block.
            // - "text_delta" → TextDelta event (print to terminal)
            // - "input_json_delta" → accumulate in the tool buffer
            "content_block_delta" => {
                let index = json["index"].as_u64()? as usize;
                let delta = &json["delta"];
                let delta_type = delta["type"].as_str()?;

                match delta_type {
                    "thinking_delta" => {
                        let text = delta["thinking"].as_str()?.to_string();
                        Some(StreamEvent::ThinkingDelta(text))
                    }
                    "text_delta" => {
                        let text = delta["text"].as_str()?.to_string();
                        // Check if this text block is inside a thinking content block.
                        if self.thinking_blocks.contains(&index) {
                            Some(StreamEvent::ThinkingDelta(text))
                        } else {
                            Some(StreamEvent::TextDelta(text))
                        }
                    }
                    "input_json_delta" => {
                        let partial = delta["partial_json"].as_str()?;

                        // Append to the accumulation buffer, guarding against
                        // unbounded growth from a runaway stream.
                        if let Some(buf) = self.tool_buffers.get_mut(&index) {
                            if buf.json.len() + partial.len() > MAX_TOOL_JSON {
                                return Some(StreamEvent::TextDelta(
                                    "[error: tool input exceeded 10 MB limit]".into(),
                                ));
                            }
                            buf.json.push_str(partial);
                        }

                        Some(StreamEvent::ToolUseInputDelta(partial.to_string()))
                    }
                    _ => None,
                }
            }

            // -- content_block_stop --
            //
            // A content block is complete.  If it was a tool_use block,
            // parse the accumulated JSON and emit ToolUseComplete.
            "content_block_stop" => {
                let index = json["index"].as_u64()? as usize;

                if let Some(buf) = self.tool_buffers.remove(&index) {
                    return match finalize_tool_call(buf) {
                        Ok(event) => Some(event),
                        Err(e) => {
                            tracing::error!(error = %e, "tool call finalization failed");
                            None
                        }
                    };
                }

                None
            }

            // -- message_delta --
            //
            // The message-level delta carries the stop_reason and usage.
            "message_delta" => {
                let stop_str = json["delta"]["stop_reason"].as_str()?;
                let stop_reason = match stop_str {
                    "end_turn" => StopReason::EndTurn,
                    "tool_use" => StopReason::ToolUse,
                    "max_tokens" => StopReason::MaxTokens,
                    other => {
                        tracing::warn!(stop_reason = other, "unknown stop reason");
                        StopReason::EndTurn
                    }
                };

                let output_tokens = json["usage"]["output_tokens"].as_u64().map(|n| n as usize);

                Some(StreamEvent::MessageComplete { stop_reason, output_tokens })
            }

            // -- error --
            //
            // The API sent an error event mid-stream (e.g., overloaded).
            "error" => {
                let message = json["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown error")
                    .to_string();
                Some(StreamEvent::Error(DysonError::Llm(message)))
            }

            // -- message_start, message_stop, ping --
            //
            // message_start: we don't need the message ID or usage yet.
            // message_stop: the stream naturally ends after this.
            // ping: keepalive, ignore.
            _ => None,
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: feed SSE lines through the parser and collect events.
    fn parse_sse(lines: &str) -> Vec<Result<StreamEvent>> {
        let mut parser = SseParser::new();
        parser.feed(lines.as_bytes())
    }

    #[test]
    fn parse_text_delta() {
        let events = parse_sse(
            "event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n"
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
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\n"
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
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n"
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
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n"
        );
        assert_eq!(events.len(), 1);
        match events[0].as_ref().unwrap() {
            StreamEvent::Error(DysonError::Llm(msg)) => assert_eq!(msg, "Overloaded"),
            other => panic!("expected Error, got: {other:?}"),
        }
    }

    #[test]
    fn handles_partial_lines() {
        // Feed bytes in two chunks that split in the middle of a line.
        let mut parser = SseParser::new();

        // First chunk: incomplete line.
        let events1 = parser.feed(b"data: {\"type\":\"content_block_del");
        assert!(events1.is_empty(), "no complete line yet");

        // Second chunk: rest of the line + newline.
        let events2 = parser.feed(
            b"ta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n"
        );
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

        let thinking: Vec<_> = events.iter().filter(|e| {
            matches!(e.as_ref().unwrap(), StreamEvent::ThinkingDelta(_))
        }).collect();
        assert_eq!(thinking.len(), 1);
        match thinking[0].as_ref().unwrap() {
            StreamEvent::ThinkingDelta(t) => assert_eq!(t, "Let me reason..."),
            other => panic!("expected ThinkingDelta, got: {other:?}"),
        }

        let text: Vec<_> = events.iter().filter(|e| {
            matches!(e.as_ref().unwrap(), StreamEvent::TextDelta(_))
        }).collect();
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
             data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_123\",\"role\":\"assistant\"}}\n\n"
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
        let mut parser = SseParser::new();

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
        let mut parser = SseParser::new();

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
        let mut parser = SseParser::new();

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
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\n"
        );
        assert_eq!(events.len(), 1);
        match events[0].as_ref().unwrap() {
            StreamEvent::MessageComplete { stop_reason, output_tokens } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
                assert_eq!(*output_tokens, Some(42));
            }
            other => panic!("expected MessageComplete, got: {other:?}"),
        }
    }

    #[test]
    fn message_delta_without_usage_has_none_tokens() {
        let events = parse_sse(
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n"
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
        let mut parser = SseParser::new();

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
                let has_error = events.iter().any(|e| {
                    matches!(e, Ok(StreamEvent::TextDelta(t)) if t.contains("10 MB"))
                });
                assert!(
                    has_error,
                    "expected tool buffer overflow error on chunk {i}"
                );
            }
        }
    }
}
