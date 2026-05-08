// ===========================================================================
// OpenAI-compatible client — wraps OpenAiClient with dialect support.
//
// For non-OpenAI endpoints (Ollama, vLLM, Together, Groq, etc.) that use
// the OpenAI Chat Completions API format but may host models requiring
// dialect-specific tool call handling (e.g., Gemma).
//
// This client delegates all HTTP/SSE work to `OpenAiClient` and adds:
//   1. Dialect detection via `text_tool_handler_for_model()`
//   2. System prompt augmentation with tool definitions
//   3. `TextToolExtractorStream` wrapping for text-based tool extraction
//
// When no dialect is needed, the request passes through unchanged.
// ===========================================================================

use async_trait::async_trait;
use std::collections::HashMap;

use crate::auth::Auth;
use crate::error::Result;
use crate::llm::dialects::{TextToolExtractorStream, deepseek, text_tool_handler_for_model};
use crate::llm::openai::{OpenAiClient, OpenAiJsonParser, message_to_openai};
use crate::llm::sse_parser::BaseSseParser;
use crate::llm::{
    CompletionConfig, LlmClient, StreamResponse, ToolDefinition, concat_system_prompt,
};
use crate::message::{ContentBlock, Message, Role};

// ---------------------------------------------------------------------------
// OpenAiCompatClient
// ---------------------------------------------------------------------------

/// OpenAI-compatible API client with dialect support.
///
/// Used for non-OpenAI endpoints that speak the Chat Completions protocol
/// but may host models (like Gemma) that need text-based tool call handling.
///
/// For actual OpenAI endpoints, use [`OpenAiClient`] directly — OpenAI's
/// own models support structured tool calls natively.
pub struct OpenAiCompatClient {
    inner: OpenAiClient,
}

impl OpenAiCompatClient {
    /// Create a new client with an API key string.
    pub fn new(api_key: &str, base_url: &str) -> Self {
        Self {
            inner: OpenAiClient::with_base_url(
                Box::new(crate::auth::BearerTokenAuth::new(api_key.to_string())),
                base_url,
            ),
        }
    }

    /// Create a new client with a custom `Auth` implementation.
    pub fn with_auth(auth: Box<dyn Auth>, base_url: &str) -> Self {
        Self {
            inner: OpenAiClient::with_base_url(auth, base_url),
        }
    }
}

#[async_trait]
impl LlmClient for OpenAiCompatClient {
    async fn stream(
        &self,
        messages: &[Message],
        system: &str,
        system_suffix: &str,
        tools: &[ToolDefinition],
        config: &CompletionConfig,
    ) -> Result<StreamResponse> {
        // Check if this model needs dialect-specific tool handling.
        if let Some(handler) = text_tool_handler_for_model(&config.model) {
            let augmented_system = if !tools.is_empty() {
                let mut s = system.to_string();
                s.push_str(&handler.format_tools_for_prompt(tools));
                s
            } else {
                system.to_string()
            };

            let messages_json =
                build_text_tool_messages_json(messages, &augmented_system, system_suffix);

            // Send with empty tools array and plain-text tool history — the
            // model gets tool info via the augmented system prompt instead.
            let mut response = self
                .inner
                .stream_with_messages_json(
                    messages_json,
                    &[],
                    config,
                    BaseSseParser::new(OpenAiJsonParser::new()),
                )
                .await?;

            // Wrap the stream to extract text-based tool calls.
            response.stream = Box::pin(TextToolExtractorStream::new(response.stream, handler));
            return Ok(response);
        }

        // DeepSeek's thinking mode:
        //   - inbound: captures `delta.reasoning` (OpenRouter's field) as
        //     Thinking via a wrapping SSE parser
        //   - outbound: echoes `reasoning_content` back on assistant turns,
        //     required by DeepSeek after a tool call
        if deepseek::is_deepseek_model(&config.model) {
            let messages_json = build_messages_json(messages, system, system_suffix, |json| {
                deepseek::inject_reasoning_content(messages, json);
            });
            let parser = BaseSseParser::new(deepseek::DeepSeekJsonParser::new());
            return self
                .inner
                .stream_with_messages_json(messages_json, tools, config, parser)
                .await;
        }

        // No dialect needed — pass through unchanged.
        self.inner
            .stream(messages, system, system_suffix, tools, config)
            .await
    }
}

/// Serialize messages to OpenAI's Chat Completions format, with the system
/// prompt as the first entry, then give the `rewrite` closure a chance to
/// mutate the resulting JSON (used by dialects to inject fields like
/// `reasoning_content` that aren't modeled by `message_to_openai`).
fn build_messages_json<F>(
    messages: &[Message],
    system: &str,
    system_suffix: &str,
    rewrite: F,
) -> Vec<serde_json::Value>
where
    F: FnOnce(&mut [serde_json::Value]),
{
    let full_system = concat_system_prompt(system, system_suffix);
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(1 + messages.len());
    out.push(serde_json::json!({
        "role": "system",
        "content": full_system,
    }));
    for msg in messages {
        out.push(message_to_openai(msg));
    }
    rewrite(&mut out);
    out
}

/// Serialize history for models whose tool protocol is plain text rather than
/// OpenAI's structured `tool_calls` / `role: tool` protocol.
fn build_text_tool_messages_json(
    messages: &[Message],
    system: &str,
    system_suffix: &str,
) -> Vec<serde_json::Value> {
    let full_system = concat_system_prompt(system, system_suffix);
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(1 + messages.len());
    out.push(serde_json::json!({
        "role": "system",
        "content": full_system,
    }));

    let mut tool_names_by_id = HashMap::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        out.push(serde_json::json!({
            "role": role,
            "content": text_tool_message_content(msg, &mut tool_names_by_id),
        }));
    }

    out
}

fn text_tool_message_content(
    msg: &Message,
    tool_names_by_id: &mut HashMap<String, String>,
) -> String {
    let mut parts = Vec::new();

    for block in &msg.content {
        match block {
            ContentBlock::Text { text } => {
                if !text.is_empty() {
                    parts.push(text.clone());
                }
            }
            ContentBlock::ToolUse { id, name, input } => {
                tool_names_by_id.insert(id.clone(), name.clone());
                parts.push(format_tool_call_transcript(id, name, input));
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let name = tool_names_by_id.get(tool_use_id).map(String::as_str);
                parts.push(format_tool_result_transcript(
                    tool_use_id,
                    name,
                    content,
                    *is_error,
                ));
            }
            ContentBlock::Document { extracted_text, .. } => {
                if !extracted_text.is_empty() {
                    parts.push(format!("Document text:\n{extracted_text}"));
                }
            }
            ContentBlock::Image { media_type, .. } => {
                parts.push(format!("[Image attached: {media_type}]"));
            }
            ContentBlock::Thinking { .. } | ContentBlock::Artefact { .. } => {}
        }
    }

    parts.join("\n\n")
}

fn format_tool_call_transcript(id: &str, name: &str, input: &serde_json::Value) -> String {
    let input_json = serde_json::to_string(input).unwrap_or_else(|_| "null".to_string());
    format!("Tool call {id} ({name}):\n{input_json}")
}

fn format_tool_result_transcript(
    tool_use_id: &str,
    name: Option<&str>,
    content: &str,
    is_error: bool,
) -> String {
    let status = if is_error { "error" } else { "ok" };
    match name {
        Some(name) => format!("Tool result for {tool_use_id} ({name}) [{status}]:\n{content}"),
        None => format!("Tool result for {tool_use_id} [{status}]:\n{content}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_tool_history_is_plain_text_not_openai_tool_protocol() {
        let messages = vec![
            Message::user("send it to me as a file"),
            Message::assistant(vec![
                ContentBlock::Text {
                    text: "I'll create the file.".into(),
                },
                ContentBlock::ToolUse {
                    id: "text_call_write_file_0".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "file_path": "report.md",
                        "content": "hello"
                    }),
                },
            ]),
            Message::tool_result("text_call_write_file_0", "wrote report.md", false),
        ];

        let json = build_text_tool_messages_json(&messages, "system", "");

        assert_eq!(json[2]["role"], "assistant");
        assert!(
            json[2].get("tool_calls").is_none(),
            "text-tool history must not include structured OpenAI tool_calls"
        );
        assert!(
            json[2]["content"]
                .as_str()
                .unwrap()
                .contains("Tool call text_call_write_file_0 (write_file):")
        );
        assert_eq!(json[3]["role"], "user");
        assert!(
            json[3]["content"]
                .as_str()
                .unwrap()
                .contains("Tool result for text_call_write_file_0 (write_file) [ok]:")
        );
    }
}
