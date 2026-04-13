// ===========================================================================
// Gemini client — streaming SSE implementation of Google's generateContent API.
//
// How Gemini streaming differs from OpenAI:
//
//   OpenAI:
//     - Endpoint: /v1/chat/completions
//     - SSE via {"stream": true} in body
//     - Auth: Authorization: Bearer header
//     - Tool calls in delta.tool_calls[] with index
//     - System prompt as role "system" message
//     - Stream end: [DONE] sentinel
//
//   Gemini:
//     - Endpoint: /v1beta/models/{model}:streamGenerateContent?alt=sse
//     - SSE via ?alt=sse query parameter
//     - Auth: x-goog-api-key header
//     - Tool calls as functionCall parts in candidates
//     - System prompt as top-level system_instruction field
//     - Roles: "user" and "model" (not "assistant")
//     - Tool results: role "function" with functionResponse parts
//     - Stream end: last chunk has finishReason set
//
// SSE format (Gemini):
//
//   data: {"candidates":[{"content":{"parts":[{"text":"Hello"}],"role":"model"}}]}
//
//   data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"bash","args":{"command":"ls"}}}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"candidatesTokenCount":42}}
//
// ===========================================================================

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::llm::sse_parser::{BaseSseParser, SseJsonParser, ToolBufferContext};
use crate::llm::stream::{StopReason, StreamEvent};
use crate::llm::{CompletionConfig, LlmClient, ToolDefinition, concat_system_prompt};
use crate::message::{ContentBlock, Message, Role};

// ---------------------------------------------------------------------------
// GeminiClient
// ---------------------------------------------------------------------------

pub struct GeminiClient {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl GeminiClient {
    pub fn new(api_key: &str, base_url: Option<&str>) -> Self {
        Self {
            client: crate::http::client().clone(),
            api_key: api_key.to_string(),
            base_url: base_url
                .unwrap_or("https://generativelanguage.googleapis.com")
                .trim_end_matches('/')
                .to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// LlmClient implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmClient for GeminiClient {
    async fn stream(
        &self,
        messages: &[Message],
        system: &str,
        system_suffix: &str,
        tools: &[ToolDefinition],
        config: &CompletionConfig,
    ) -> Result<crate::llm::StreamResponse> {
        // -- Build contents array --
        let mut contents: Vec<serde_json::Value> = Vec::new();
        for msg in messages {
            contents.push(message_to_gemini(msg));
        }

        // -- Build request body --
        let mut body = serde_json::json!({
            "contents": contents,
            "generationConfig": {
                "maxOutputTokens": config.max_tokens,
            }
        });

        // System instruction.
        let full_system = concat_system_prompt(system, system_suffix);
        if !full_system.is_empty() {
            body["system_instruction"] = serde_json::json!({
                "parts": [{ "text": full_system }]
            });
        }

        // Temperature.
        if let Some(temp) = config.temperature {
            body["generationConfig"]["temperature"] = serde_json::json!(temp);
        }

        // Tools.
        if !tools.is_empty() {
            let decls: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": sanitize_schema_for_gemini(&t.input_schema),
                    })
                })
                .collect();
            body["tools"] = serde_json::json!([{
                "function_declarations": decls
            }]);
        }

        // -- Send request --
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.base_url, config.model,
        );

        tracing::debug!(
            model = config.model,
            message_count = messages.len(),
            "sending Gemini streaming request"
        );

        let response = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(crate::llm::map_http_error(response, "Gemini").await);
        }

        Ok(crate::llm::build_stream_response(
            response,
            BaseSseParser::new(GeminiJsonParser::new()),
        ))
    }
}

// ---------------------------------------------------------------------------
// Schema sanitization — strip fields unsupported by Gemini.
// ---------------------------------------------------------------------------

/// Recursively remove JSON Schema fields that Gemini's function declaration
/// parameters don't support.
///
/// Gemini's Schema proto (v1beta) supports: `type`, `format`, `title`,
/// `description`, `nullable`, `enum`, `items`, `maxItems`, `minItems`,
/// `properties`, `required`, `minProperties`, `maxProperties`, `minimum`,
/// `maximum`, `minLength`, `maxLength`, `pattern`, `example` (singular),
/// `anyOf`, `propertyOrdering`, and `default`.
///
/// Standard JSON Schema fields like `additionalProperties`, `$schema`,
/// `$ref`, `allOf`, `oneOf`, and `not` are NOT in the proto and cause
/// 400 Bad Request errors.
///
/// This function strips those unsupported fields so tool schemas authored
/// for OpenAI/Anthropic work with Gemini without modification.
fn sanitize_schema_for_gemini(schema: &serde_json::Value) -> serde_json::Value {
    const UNSUPPORTED: &[&str] = &[
        "additionalProperties",
        "$schema",
        "$ref",
        "$defs",
        "$id",
        "examples", // note: singular "example" IS supported
        "allOf",
        "oneOf",
        "not",
        "readOnly",
        "writeOnly",
        "const",
    ];

    match schema {
        serde_json::Value::Object(map) => {
            let mut cleaned = serde_json::Map::new();
            for (key, value) in map {
                if UNSUPPORTED.contains(&key.as_str()) {
                    continue;
                }
                // Recurse into nested schemas (properties values, items).
                cleaned.insert(key.clone(), sanitize_schema_for_gemini(value));
            }
            serde_json::Value::Object(cleaned)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(sanitize_schema_for_gemini).collect())
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Message serialization — internal format → Gemini format.
// ---------------------------------------------------------------------------

/// Convert an internal Message to Gemini's JSON format.
///
/// Key differences from OpenAI:
/// - Assistant role is "model" (not "assistant")
/// - Tool results use role "function" with functionResponse parts
/// - Tool calls are functionCall parts (not a separate tool_calls array)
/// - Images use inlineData (not image_url)
fn message_to_gemini(msg: &Message) -> serde_json::Value {
    // Tool result → role "function" with functionResponse parts.
    if let Some(ContentBlock::ToolResult {
        tool_use_id,
        content,
        ..
    }) = msg.content.first()
    {
        if matches!(msg.role, Role::User) {
            return serde_json::json!({
                "role": "function",
                "parts": [{
                    "functionResponse": {
                        "name": tool_use_id,
                        "response": {
                            "content": content
                        }
                    }
                }]
            });
        }
    }

    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "model",
    };

    let parts: Vec<serde_json::Value> = msg
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(serde_json::json!({ "text": text })),
            ContentBlock::ToolUse { name, input, .. } => Some(serde_json::json!({
                "functionCall": {
                    "name": name,
                    "args": input,
                }
            })),
            ContentBlock::Image { data, media_type } => Some(serde_json::json!({
                "inlineData": {
                    "mimeType": media_type,
                    "data": data,
                }
            })),
            ContentBlock::Document { extracted_text, .. } => {
                Some(serde_json::json!({ "text": extracted_text }))
            }
            // Thinking blocks are internal — skip.
            ContentBlock::Thinking { .. } => None,
            // ToolResult handled above at the message level.
            ContentBlock::ToolResult { .. } => None,
        })
        .collect();

    serde_json::json!({
        "role": role,
        "parts": parts,
    })
}

// ---------------------------------------------------------------------------
// Gemini SSE Parser
// ---------------------------------------------------------------------------

/// Gemini-specific SSE JSON parser.
///
/// Handles Gemini's candidates[].content.parts[] event model.
struct GeminiJsonParser {
    completed: bool,
    /// Counter for generating tool call IDs when the API doesn't provide them.
    tool_index: usize,
}

impl GeminiJsonParser {
    fn new() -> Self {
        Self {
            completed: false,
            tool_index: 0,
        }
    }
}

impl SseJsonParser for GeminiJsonParser {
    fn parse_json(
        &mut self,
        json: &serde_json::Value,
        ctx: &mut ToolBufferContext,
    ) -> Vec<Result<StreamEvent>> {
        let mut events = Vec::new();

        let Some(candidates) = json["candidates"].as_array() else {
            return events;
        };

        for candidate in candidates {
            // -- Content parts --
            if let Some(parts) = candidate["content"]["parts"].as_array() {
                for part in parts {
                    // Text content.
                    if let Some(text) = part["text"].as_str() {
                        if !text.is_empty() {
                            events.push(Ok(StreamEvent::TextDelta(text.to_string())));
                        }
                    }

                    // Function call.
                    if let Some(fc) = part.get("functionCall") {
                        let name = fc["name"].as_str().unwrap_or("").to_string();
                        // Gemini may provide an "id" field; fall back to generated ID.
                        let id = fc["id"]
                            .as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| format!("gemini_call_{}", self.tool_index));

                        let args = &fc["args"];
                        let index = self.tool_index;
                        self.tool_index += 1;

                        // For Gemini, function calls arrive complete (not streamed
                        // in fragments like OpenAI), so we start + finalize in one go.
                        if let Some(err) =
                            ctx.start_tool(index, id.clone(), name.clone())
                        {
                            events.push(Err(DysonError::Llm(format!("{err:?}"))));
                            return events;
                        }

                        events.push(Ok(StreamEvent::ToolUseStart {
                            id: id.clone(),
                            name: name.clone(),
                        }));

                        // Append the full args JSON and finalize immediately.
                        let args_str = args.to_string();
                        if let Some(err_event) = ctx.append_tool_json(index, &args_str) {
                            events.push(Ok(err_event));
                        }

                        events.extend(ctx.finalize_tool(index));
                    }
                }
            }

            // -- Finish reason --
            if let Some(reason) = candidate["finishReason"].as_str()
                && !self.completed
            {
                self.completed = true;

                let output_tokens = json["usageMetadata"]["candidatesTokenCount"]
                    .as_u64()
                    .map(|n| n as usize);

                let stop_reason = match reason {
                    "MAX_TOKENS" => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                };

                events.extend(ctx.drain_all());
                events.push(Ok(StreamEvent::MessageComplete {
                    stop_reason,
                    output_tokens,
                }));
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
    use crate::llm::SseStreamParser;

    fn parse_sse(lines: &str) -> Vec<Result<StreamEvent>> {
        let mut parser = BaseSseParser::new(GeminiJsonParser::new());
        parser.feed(lines.as_bytes())
    }

    #[test]
    fn parse_text_delta() {
        let events = parse_sse(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"}}]}\n\n",
        );
        assert_eq!(events.len(), 1);
        match events[0].as_ref().unwrap() {
            StreamEvent::TextDelta(text) => assert_eq!(text, "Hello"),
            other => panic!("expected TextDelta, got: {other:?}"),
        }
    }

    #[test]
    fn parse_function_call() {
        let sse = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"bash\",\"args\":{\"command\":\"ls\"}}}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"candidatesTokenCount\":10}}\n\n";
        let events = parse_sse(sse);
        // Should produce: ToolUseStart, ToolUseComplete, MessageComplete
        let mut found_start = false;
        let mut found_complete = false;
        let mut found_msg_complete = false;
        for event in &events {
            match event.as_ref().unwrap() {
                StreamEvent::ToolUseStart { name, .. } => {
                    assert_eq!(name, "bash");
                    found_start = true;
                }
                StreamEvent::ToolUseComplete { name, input, .. } => {
                    assert_eq!(name, "bash");
                    assert_eq!(input["command"], "ls");
                    found_complete = true;
                }
                StreamEvent::MessageComplete {
                    stop_reason,
                    output_tokens,
                } => {
                    assert_eq!(*stop_reason, StopReason::EndTurn);
                    assert_eq!(*output_tokens, Some(10));
                    found_msg_complete = true;
                }
                _ => {}
            }
        }
        assert!(found_start, "missing ToolUseStart");
        assert!(found_complete, "missing ToolUseComplete");
        assert!(found_msg_complete, "missing MessageComplete");
    }

    #[test]
    fn parse_finish_max_tokens() {
        let sse = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"truncated\"}],\"role\":\"model\"},\"finishReason\":\"MAX_TOKENS\"}]}\n\n";
        let events = parse_sse(sse);
        let msg_complete = events
            .iter()
            .find_map(|e| match e.as_ref().ok()? {
                StreamEvent::MessageComplete { stop_reason, .. } => Some(stop_reason),
                _ => None,
            })
            .expect("should have MessageComplete");
        assert_eq!(*msg_complete, StopReason::MaxTokens);
    }

    #[test]
    fn message_user_text() {
        let msg = Message::user("hello");
        let json = message_to_gemini(&msg);
        assert_eq!(json["role"], "user");
        assert_eq!(json["parts"][0]["text"], "hello");
    }

    #[test]
    fn message_assistant_text() {
        let msg = Message::assistant(vec![ContentBlock::Text {
            text: "hi".into(),
        }]);
        let json = message_to_gemini(&msg);
        assert_eq!(json["role"], "model");
        assert_eq!(json["parts"][0]["text"], "hi");
    }

    #[test]
    fn message_tool_result() {
        let msg = Message::tool_result("bash", "output", false);
        let json = message_to_gemini(&msg);
        assert_eq!(json["role"], "function");
        assert_eq!(json["parts"][0]["functionResponse"]["name"], "bash");
        assert_eq!(
            json["parts"][0]["functionResponse"]["response"]["content"],
            "output"
        );
    }

    #[test]
    fn message_with_tool_use() {
        let msg = Message::assistant(vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        }]);
        let json = message_to_gemini(&msg);
        assert_eq!(json["role"], "model");
        assert_eq!(json["parts"][0]["functionCall"]["name"], "bash");
        assert_eq!(json["parts"][0]["functionCall"]["args"]["command"], "ls");
    }

    #[test]
    fn message_with_image() {
        let msg = Message::user_multimodal(vec![
            ContentBlock::Text {
                text: "What is this?".into(),
            },
            ContentBlock::Image {
                data: "abc123".into(),
                media_type: "image/jpeg".into(),
            },
        ]);
        let json = message_to_gemini(&msg);
        assert_eq!(json["parts"][0]["text"], "What is this?");
        assert_eq!(json["parts"][1]["inlineData"]["mimeType"], "image/jpeg");
        assert_eq!(json["parts"][1]["inlineData"]["data"], "abc123");
    }

    // -----------------------------------------------------------------------
    // sanitize_schema_for_gemini tests
    // -----------------------------------------------------------------------

    #[test]
    fn sanitize_strips_additional_properties() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"],
            "additionalProperties": false
        });
        let cleaned = sanitize_schema_for_gemini(&schema);
        assert!(cleaned.get("additionalProperties").is_none());
        assert_eq!(cleaned["type"], "object");
        assert_eq!(cleaned["required"][0], "name");
        assert_eq!(cleaned["properties"]["name"]["type"], "string");
    }

    #[test]
    fn sanitize_strips_nested_unsupported_fields() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "config": {
                    "type": "object",
                    "properties": {
                        "verbose": { "type": "boolean", "const": true }
                    },
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        });
        let cleaned = sanitize_schema_for_gemini(&schema);
        assert!(cleaned.get("additionalProperties").is_none());
        let config = &cleaned["properties"]["config"];
        assert!(config.get("additionalProperties").is_none());
        assert!(config["properties"]["verbose"].get("const").is_none());
        assert_eq!(config["properties"]["verbose"]["type"], "boolean");
    }

    #[test]
    fn sanitize_preserves_supported_fields() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "count": {
                    "type": "number",
                    "description": "Item count",
                    "minimum": 0,
                    "maximum": 100
                }
            },
            "required": ["count"]
        });
        let cleaned = sanitize_schema_for_gemini(&schema);
        assert_eq!(cleaned, schema);
    }

    #[test]
    fn sanitize_handles_non_object_schema() {
        let schema = serde_json::json!("string");
        let cleaned = sanitize_schema_for_gemini(&schema);
        assert_eq!(cleaned, schema);
    }
}
