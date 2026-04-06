// ===========================================================================
// Ollama Cloud client — OpenAI-compatible provider for ollama.com cloud models.
//
// Ollama Cloud (https://ollama.com) hosts models in the cloud, accessible via
// an OpenAI-compatible Chat Completions API with Bearer token authentication.
//
// This client adds:
//   1. Default base URL: https://ollama.com
//   2. Bearer token auth via OLLAMA_API_KEY
//
// Dialect support (e.g., Gemma text-based tool calls) is handled by the
// underlying `OpenAiCompatClient`.
// ===========================================================================

use async_trait::async_trait;

use crate::auth::{Auth, BearerTokenAuth};
use crate::error::Result;
use crate::llm::openai_compat::OpenAiCompatClient;
use crate::llm::{CompletionConfig, LlmClient, StreamResponse, ToolDefinition};
use crate::message::Message;

/// Default Ollama Cloud API base URL.
const DEFAULT_BASE_URL: &str = "https://ollama.com";

// ---------------------------------------------------------------------------
// OllamaCloudClient
// ---------------------------------------------------------------------------

/// Ollama Cloud API client — delegates to [`OpenAiCompatClient`] with Ollama Cloud defaults.
pub struct OllamaCloudClient {
    inner: OpenAiCompatClient,
}

impl OllamaCloudClient {
    pub fn new(api_key: &str) -> Self {
        let auth: Box<dyn Auth> = Box::new(BearerTokenAuth::new(api_key.to_string()));

        Self {
            inner: OpenAiCompatClient::with_auth(auth, Some(DEFAULT_BASE_URL)),
        }
    }
}

#[async_trait]
impl LlmClient for OllamaCloudClient {
    async fn stream(
        &self,
        messages: &[Message],
        system: &str,
        system_suffix: &str,
        tools: &[ToolDefinition],
        config: &CompletionConfig,
    ) -> Result<StreamResponse> {
        self.inner
            .stream(messages, system, system_suffix, tools, config)
            .await
    }
}
