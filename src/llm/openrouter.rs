// ===========================================================================
// OpenRouter client — thin wrapper over the OpenAI-compatible client.
//
// OpenRouter (https://openrouter.ai) provides a unified API for 200+ models
// using the OpenAI Chat Completions format.  This client delegates all
// streaming and parsing to `OpenAiClient`, adding only:
//
//   1. Default base URL: https://openrouter.ai/api
//   2. Optional `HTTP-Referer` and `X-Title` headers (OpenRouter recommends
//      these for app attribution and priority routing).
//
// Because OpenRouter is fully OpenAI-compatible, no custom SSE parser or
// message serialization is needed — `OpenAiClient` handles everything.
// ===========================================================================

use async_trait::async_trait;

use crate::auth::{Auth, BearerTokenAuth, CompositeAuth, StaticHeadersAuth};
use crate::error::Result;
use crate::llm::openai::OpenAiClient;
use crate::llm::{CompletionConfig, LlmClient, StreamResponse, ToolDefinition};
use crate::message::Message;

/// Default OpenRouter API base URL.
const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api";

// ---------------------------------------------------------------------------
// OpenRouterClient
// ---------------------------------------------------------------------------

/// OpenRouter API client — delegates to [`OpenAiClient`] with OpenRouter defaults.
pub struct OpenRouterClient {
    inner: OpenAiClient,
}

impl OpenRouterClient {
    /// Create a new OpenRouter client.
    ///
    /// Builds a `CompositeAuth` that layers:
    ///   1. `BearerTokenAuth` — the OpenRouter API key
    ///   2. `StaticHeadersAuth` — `HTTP-Referer` and `X-Title` (if provided)
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
            inner: OpenAiClient::with_auth(auth, Some(base_url.unwrap_or(DEFAULT_BASE_URL))),
        }
    }
}

#[async_trait]
impl LlmClient for OpenRouterClient {
    async fn stream(
        &self,
        messages: &[Message],
        system: &str,
        tools: &[ToolDefinition],
        config: &CompletionConfig,
    ) -> Result<StreamResponse> {
        self.inner.stream(messages, system, tools, config).await
    }
}
