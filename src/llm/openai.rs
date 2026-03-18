// ===========================================================================
// OpenAI client — streaming SSE implementation of the Chat Completions API.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements `LlmClient` for the OpenAI Chat Completions API.  This
//   covers OpenAI proper (GPT-4o, o3, etc.), and any OpenAI-compatible
//   endpoint (local models via Ollama, vLLM, Together, Groq, etc.).
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

use std::collections::HashMap;
use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use tokio_stream::StreamExt;

use crate::error::{DysonError, Result};
use crate::llm::stream::{StopReason, StreamEvent};
use crate::llm::{CompletionConfig, LlmClient, ToolDefinition};
use crate::message::{ContentBlock, Message, Role};

// ---------------------------------------------------------------------------
// OpenAiClient
// ---------------------------------------------------------------------------

/// OpenAI Chat Completions API client with SSE streaming.
///
/// Works with any OpenAI-compatible endpoint — just change the base_url:
/// - OpenAI: `https://api.openai.com` (default)
/// - Azure OpenAI: `https://<resource>.openai.azure.com/openai/deployments/<model>`
/// - Ollama: `http://localhost:11434`
/// - Together: `https://api.together.xyz`
/// - vLLM: `http://localhost:8000`
pub struct OpenAiClient {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl OpenAiClient {
    pub fn new(api_key: &str, base_url: Option<&str>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.to_string(),
            base_url: base_url
                .unwrap_or("https://api.openai.com")
                .trim_end_matches('/')
                .to_string(),
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
        tools: &[ToolDefinition],
        config: &CompletionConfig,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        // -- Build messages array --
        //
        // OpenAI puts the system prompt as the first message with role "system".
        // Then user/assistant messages follow.  Tool results use role "tool"
        // with a tool_call_id field (different from Anthropic's approach of
        // putting them in user messages).
        let mut messages_json: Vec<serde_json::Value> = Vec::new();

        // System message first.
        messages_json.push(serde_json::json!({
            "role": "system",
            "content": system,
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

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "failed to read error body".into());
            return Err(DysonError::Llm(format!(
                "OpenAI API returned {status}: {body}"
            )));
        }

        // -- Parse SSE stream --
        let byte_stream = response.bytes_stream();

        let event_stream = async_stream::stream! {
            let mut parser = OpenAiSseParser::new();

            tokio::pin!(byte_stream);

            while let Some(chunk_result) = byte_stream.next().await {
                match chunk_result {
                    Ok(bytes) => {
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
// Message serialization — internal format → OpenAI format.
// ---------------------------------------------------------------------------

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
    {
        if matches!(msg.role, Role::User) && matches!(msg.content[0], ContentBlock::ToolResult { .. })
        {
            return serde_json::json!({
                "role": "tool",
                "tool_call_id": tool_use_id,
                "content": content,
            });
        }
    }

    // Check if this is an assistant message with tool calls.
    let has_tool_uses = msg
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { .. }));

    if has_tool_uses {
        // Extract text content (if any) and tool calls separately.
        let text: String = msg
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

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

    // Simple text message.
    let text: String = msg
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    serde_json::json!({
        "role": role_str,
        "content": text,
    })
}

// ---------------------------------------------------------------------------
// OpenAI SSE Parser
// ---------------------------------------------------------------------------

/// Parses OpenAI's SSE stream into StreamEvents.
///
/// Similar structure to the Anthropic parser but handles OpenAI's different
/// JSON format: choices[0].delta instead of content_block events.
struct OpenAiSseParser {
    /// Buffer for incomplete SSE lines.
    line_buffer: String,

    /// Active tool call accumulation.
    /// Key: tool_calls array index.
    /// Value: (call_id, function_name, accumulated_arguments_string).
    tool_buffers: HashMap<usize, ToolCallBuffer>,
}

struct ToolCallBuffer {
    id: String,
    name: String,
    arguments: String,
}

impl OpenAiSseParser {
    fn new() -> Self {
        Self {
            line_buffer: String::new(),
            tool_buffers: HashMap::new(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Vec<Result<StreamEvent>> {
        let mut events = Vec::new();
        self.line_buffer.push_str(&String::from_utf8_lossy(bytes));

        while let Some(newline_pos) = self.line_buffer.find('\n') {
            let line: String = self.line_buffer.drain(..=newline_pos).collect();
            let line = line.trim();

            if line.is_empty() || line.starts_with(':') {
                continue;
            }

            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();

                // [DONE] signals end of stream.
                if data == "[DONE]" {
                    // Flush any remaining tool buffers.
                    // OpenAI emits finish_reason before [DONE], so tool
                    // buffers should already be flushed.  But just in case:
                    let remaining: Vec<ToolCallBuffer> =
                        self.tool_buffers.drain().map(|(_, buf)| buf).collect();
                    for buf in remaining {
                        events.push(finalize_tool_call(buf));
                    }
                    continue;
                }

                match serde_json::from_str::<serde_json::Value>(data) {
                    Ok(json) => {
                        let new_events = self.parse_chunk(&json);
                        events.extend(new_events);
                    }
                    Err(e) => {
                        tracing::warn!(data = data, error = %e, "failed to parse OpenAI SSE JSON");
                    }
                }
            }
        }

        events
    }

    /// Parse a single OpenAI SSE chunk.
    ///
    /// OpenAI chunks have this structure:
    /// ```json
    /// {
    ///   "choices": [{
    ///     "index": 0,
    ///     "delta": {
    ///       "content": "Hello",           // text content
    ///       "tool_calls": [...]            // tool calls
    ///     },
    ///     "finish_reason": "stop"          // or "tool_calls" or null
    ///   }]
    /// }
    /// ```
    fn parse_chunk(&mut self, json: &serde_json::Value) -> Vec<Result<StreamEvent>> {
        let mut events = Vec::new();

        let Some(choices) = json["choices"].as_array() else {
            return events;
        };

        for choice in choices {
            let delta = &choice["delta"];

            // -- Text content --
            if let Some(text) = delta["content"].as_str() {
                if !text.is_empty() {
                    events.push(Ok(StreamEvent::TextDelta(text.to_string())));
                }
            }

            // -- Tool calls --
            if let Some(tool_calls) = delta["tool_calls"].as_array() {
                for tc in tool_calls {
                    let index = tc["index"].as_u64().unwrap_or(0) as usize;

                    // First chunk for this tool call has id and function.name.
                    if let Some(id) = tc["id"].as_str() {
                        let name = tc["function"]["name"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();

                        self.tool_buffers.insert(
                            index,
                            ToolCallBuffer {
                                id: id.to_string(),
                                name: name.clone(),
                                arguments: String::new(),
                            },
                        );

                        events.push(Ok(StreamEvent::ToolUseStart {
                            id: id.to_string(),
                            name,
                        }));
                    }

                    // Subsequent chunks have function.arguments fragments.
                    if let Some(args) = tc["function"]["arguments"].as_str() {
                        if let Some(buf) = self.tool_buffers.get_mut(&index) {
                            buf.arguments.push_str(args);
                        }
                        events.push(Ok(StreamEvent::ToolUseInputDelta(args.to_string())));
                    }
                }
            }

            // -- Finish reason --
            if let Some(reason) = choice["finish_reason"].as_str() {
                match reason {
                    "stop" => {
                        events.push(Ok(StreamEvent::MessageComplete {
                            stop_reason: StopReason::EndTurn,
                        }));
                    }
                    "tool_calls" => {
                        // Flush all accumulated tool calls.
                        let buffers: Vec<ToolCallBuffer> =
                            self.tool_buffers.drain().map(|(_, buf)| buf).collect();
                        for buf in buffers {
                            events.push(finalize_tool_call(buf));
                        }
                        events.push(Ok(StreamEvent::MessageComplete {
                            stop_reason: StopReason::ToolUse,
                        }));
                    }
                    "length" => {
                        events.push(Ok(StreamEvent::MessageComplete {
                            stop_reason: StopReason::MaxTokens,
                        }));
                    }
                    _ => {
                        tracing::warn!(finish_reason = reason, "unknown OpenAI finish_reason");
                        events.push(Ok(StreamEvent::MessageComplete {
                            stop_reason: StopReason::EndTurn,
                        }));
                    }
                }
            }
        }

        events
    }

}

/// Parse accumulated arguments and emit ToolUseComplete.
fn finalize_tool_call(buf: ToolCallBuffer) -> Result<StreamEvent> {
    let input = match serde_json::from_str(&buf.arguments) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(
                tool = buf.name,
                arguments = buf.arguments,
                error = %e,
                "failed to parse accumulated OpenAI tool arguments"
            );
            serde_json::json!({})
        }
    };

    Ok(StreamEvent::ToolUseComplete {
        id: buf.id,
        name: buf.name,
        input,
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_sse(lines: &str) -> Vec<Result<StreamEvent>> {
        let mut parser = OpenAiSseParser::new();
        parser.feed(lines.as_bytes())
    }

    #[test]
    fn parse_text_delta() {
        let events = parse_sse(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n"
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
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
        );
        match events.last().unwrap().as_ref().unwrap() {
            StreamEvent::MessageComplete { stop_reason } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
            }
            other => panic!("expected MessageComplete, got: {other:?}"),
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
}
