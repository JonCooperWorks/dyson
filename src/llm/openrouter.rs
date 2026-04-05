// ===========================================================================
// OpenRouter client — OpenAI-compatible provider with app attribution headers.
//
// OpenRouter (https://openrouter.ai) provides a unified API for 200+ models
// using the OpenAI Chat Completions format.  This client adds:
//
//   1. Default base URL: https://openrouter.ai/api
//   2. `HTTP-Referer` and `X-Title` headers for app attribution.
//
// Dialect support (e.g., Gemma text-based tool calls) is handled by the
// underlying `OpenAiCompatClient`.
// ===========================================================================

use async_trait::async_trait;

use crate::auth::{Auth, BearerTokenAuth, CompositeAuth, StaticHeadersAuth};
use crate::error::Result;
use crate::llm::openai_compat::OpenAiCompatClient;
use crate::llm::{CompletionConfig, LlmClient, StreamResponse, ToolDefinition};
use crate::message::Message;

/// Default OpenRouter API base URL.
const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api";

// ---------------------------------------------------------------------------
// OpenRouterClient
// ---------------------------------------------------------------------------

/// OpenRouter API client — delegates to [`OpenAiCompatClient`] with OpenRouter defaults.
pub struct OpenRouterClient {
    inner: OpenAiCompatClient,
}

impl OpenRouterClient {
    pub fn new(api_key: &str, base_url: Option<&str>) -> Self {
        let mut headers = std::collections::HashMap::new();
        headers.insert(
            "HTTP-Referer".to_string(),
            "https://github.com/dyson".to_string(),
        );
        headers.insert("X-Title".to_string(), "Dyson".to_string());

        let auth: Box<dyn Auth> = Box::new(CompositeAuth::new(vec![
            Box::new(BearerTokenAuth::new(api_key.to_string())),
            Box::new(StaticHeadersAuth::new(headers)),
        ]));

        Self {
            inner: OpenAiCompatClient::with_auth(auth, Some(base_url.unwrap_or(DEFAULT_BASE_URL))),
        }
    }
}

#[async_trait]
impl LlmClient for OpenRouterClient {
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
