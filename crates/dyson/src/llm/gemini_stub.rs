// ===========================================================================
// Gemini stub — placeholder LlmClient for the provider registry.
//
// The Gemini provider is currently used only for image generation (via the
// `image_generate` tool).  This stub satisfies the registry's `create_client`
// requirement so that Gemini can appear in the `"providers"` map and have
// its API key resolved, without implementing a full chat LLM client.
//
// When Gemini chat support is added, this stub can be replaced with a real
// implementation.
// ===========================================================================

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::llm::{CompletionConfig, LlmClient, StreamResponse, ToolDefinition};
use crate::message::Message;

/// A stub LLM client for the Gemini provider.
///
/// Returns an error on every `stream()` call, directing users to configure
/// Gemini as an `image_generation_provider` instead of the main agent
/// provider.
pub struct GeminiStubClient;

#[async_trait]
impl LlmClient for GeminiStubClient {
    async fn stream(
        &self,
        _messages: &[Message],
        _system: &str,
        _system_suffix: &str,
        _tools: &[ToolDefinition],
        _config: &CompletionConfig,
    ) -> Result<StreamResponse> {
        Err(DysonError::Llm(
            "Gemini is not supported as a chat LLM provider. \
             Configure it as image_generation_provider in the agent settings \
             to use it for image generation."
                .into(),
        ))
    }
}
