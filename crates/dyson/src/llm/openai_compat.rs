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

use crate::auth::Auth;
use crate::error::Result;
use crate::llm::dialects::{deepseek, text_tool_handler_for_model, TextToolExtractorStream};
use crate::llm::openai::{message_to_openai, OpenAiClient};
use crate::llm::sse_parser::BaseSseParser;
use crate::llm::{concat_system_prompt, CompletionConfig, LlmClient, StreamResponse, ToolDefinition};
use crate::message::Message;

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

            // Send with empty tools array — the model gets tool info via
            // the augmented system prompt instead.
            let mut response = self
                .inner
                .stream(messages, &augmented_system, system_suffix, &[], config)
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
