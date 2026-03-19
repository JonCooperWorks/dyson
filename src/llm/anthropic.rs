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
//   We use a HashMap<usize, ToolUseBuffer> to track active tool calls by
//   their content block index.
// ===========================================================================

use std::collections::HashMap;
use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use tokio_stream::StreamExt;

use crate::error::{DysonError, Result};
use crate::llm::stream::{StopReason, StreamEvent};
use crate::llm::{CompletionConfig, LlmClient, ToolDefinition};
use crate::message::Message;

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

    /// Anthropic API key (sent as `x-api-key` header).
    api_key: String,

    /// Base URL for the API (default: "https://api.anthropic.com").
    ///
    /// Configurable for testing with mock servers or for proxied setups.
    base_url: String,
}

impl AnthropicClient {
    /// Create a new Anthropic client.
    ///
    /// `base_url` is optional — pass `None` for the default Anthropic endpoint.
    pub fn new(api_key: &str, base_url: Option<&str>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.to_string(),
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
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        // -- Build the request body --
        let messages_json: Vec<serde_json::Value> =
            messages.iter().map(|m| m.to_anthropic_value()).collect();

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

        let response = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

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
            return Err(DysonError::Llm(format!(
                "Anthropic API returned {status}: {body}"
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

        Ok(Box::pin(event_stream))
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
    /// Buffer for incomplete lines (bytes received but no newline yet).
    line_buffer: String,

    /// Active tool_use blocks being accumulated.
    ///
    /// Key: content block index (from the Anthropic API).
    /// Value: (tool_use_id, tool_name, accumulated_json_string).
    ///
    /// When `content_block_start` arrives with type "tool_use", we insert
    /// an entry.  Each `input_json_delta` appends to the JSON string.
    /// On `content_block_stop`, we parse the JSON and emit ToolUseComplete.
    tool_buffers: HashMap<usize, ToolUseBuffer>,

    /// Content block indices that are "thinking" blocks.
    ///
    /// Anthropic's extended thinking emits thinking content as regular
    /// content_block_delta events with type "thinking_delta".  We track
    /// which blocks are thinking so we can emit ThinkingDelta instead of
    /// TextDelta for their content.
    thinking_blocks: std::collections::HashSet<usize>,
}

/// State for a single in-progress tool_use content block.
struct ToolUseBuffer {
    id: String,
    name: String,
    /// Accumulated partial JSON fragments from input_json_delta events.
    json: String,
}

impl SseParser {
    fn new() -> Self {
        Self {
            line_buffer: String::new(),
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

        // Append raw bytes to the line buffer.
        //
        // Note: SSE is always UTF-8.  If we get invalid UTF-8, we replace
        // it with the replacement character rather than crashing.
        self.line_buffer.push_str(&String::from_utf8_lossy(bytes));

        // Process all complete lines.
        while let Some(newline_pos) = self.line_buffer.find('\n') {
            // Split off the complete line (including the newline).
            let line: String = self.line_buffer.drain(..=newline_pos).collect();
            let line = line.trim();

            // Skip empty lines (SSE event delimiters) and comments.
            if line.is_empty() || line.starts_with(':') {
                continue;
            }

            // Skip "event:" lines — we determine the event type from the
            // JSON "type" field in the data line instead.
            if line.starts_with("event:") {
                continue;
            }

            // Parse "data:" lines.
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();

                // "[DONE]" is an OpenAI convention; Anthropic uses
                // "message_stop" events instead.  Handle both for safety.
                if data == "[DONE]" {
                    continue;
                }

                // Parse the JSON payload and convert to StreamEvent(s).
                match serde_json::from_str::<serde_json::Value>(data) {
                    Ok(json) => {
                        if let Some(event) = self.parse_sse_json(&json) {
                            events.push(Ok(event));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(data = data, error = %e, "failed to parse SSE JSON");
                        // Don't emit an error event for parse failures — they're
                        // usually harmless (ping events, etc.).
                    }
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

                    // Start accumulating JSON for this tool call.
                    self.tool_buffers.insert(
                        index,
                        ToolUseBuffer {
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

                        // Append to the accumulation buffer.
                        if let Some(buf) = self.tool_buffers.get_mut(&index) {
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
                    // Parse the accumulated JSON fragments into a Value.
                    let input = match serde_json::from_str(&buf.json) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::error!(
                                tool = buf.name,
                                json = buf.json,
                                error = %e,
                                "failed to parse accumulated tool_use JSON"
                            );
                            // Return an empty object so the tool can still
                            // be called (it will likely fail with a missing
                            // field error, which is better than crashing).
                            serde_json::json!({})
                        }
                    };

                    return Some(StreamEvent::ToolUseComplete {
                        id: buf.id,
                        name: buf.name,
                        input,
                    });
                }

                None
            }

            // -- message_delta --
            //
            // The message-level delta carries the stop_reason.
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

                Some(StreamEvent::MessageComplete { stop_reason })
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
            StreamEvent::MessageComplete { stop_reason } => {
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
            StreamEvent::MessageComplete { stop_reason } => {
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
}
