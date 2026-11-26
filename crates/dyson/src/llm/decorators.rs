//! Transparent `LlmClient` decorators applied by `create_client()`:
//! retry with backoff and a per-provider concurrency cap.

use super::*;

// ---------------------------------------------------------------------------
// Retry decorator — transparent exponential backoff for every LlmClient.
//
// Every concrete client goes through `create_client()`, which wraps it in
// `RetryingLlmClient`.  Call sites (agent loop, compaction, reflection,
// quick_response, learning synthesis) all see retries automatically —
// previously only the main agent loop retried, so a 429 during compaction
// or reflection would abort that operation after a single attempt.
// ---------------------------------------------------------------------------

/// Classify an LLM error as worth retrying.
///
/// Rate limits, provider-overloaded responses, and transport errors are
/// transient.  Everything else — auth failures, malformed requests,
/// tool_use / vision rejections — will fail the same way on retry.
pub fn is_retryable(err: &DysonError) -> bool {
    matches!(
        err,
        DysonError::LlmRateLimit { .. } | DysonError::LlmOverloaded { .. } | DysonError::Http(_)
    )
}

/// Wraps an `LlmClient` with exponential-backoff retry for retryable errors.
///
/// Backoff schedule: base 1s doubled per attempt, with up to 50% jitter
/// (1s → 2s → 4s → 8s → 16s...).  `max_retries` counts retries *after* the
/// initial attempt; 0 disables retry.
pub(crate) struct RetryingLlmClient {
    inner: Box<dyn LlmClient>,
    max_retries: usize,
    base_delay_ms: u64,
}

impl RetryingLlmClient {
    pub fn new(inner: Box<dyn LlmClient>, max_retries: usize) -> Self {
        Self {
            inner,
            max_retries,
            base_delay_ms: 1000,
        }
    }

    /// Test-only constructor that lets tests run the retry loop without
    /// waiting seconds for each backoff.
    #[cfg(test)]
    pub(super) fn with_base_delay(
        inner: Box<dyn LlmClient>,
        max_retries: usize,
        base_delay_ms: u64,
    ) -> Self {
        Self {
            inner,
            max_retries,
            base_delay_ms,
        }
    }
}

#[async_trait]
impl LlmClient for RetryingLlmClient {
    async fn stream(
        &self,
        messages: &[Message],
        system: &str,
        system_suffix: &str,
        tools: &[ToolDefinition],
        tool_instances: &std::collections::HashMap<String, std::sync::Arc<dyn Tool>>,
        config: &CompletionConfig,
    ) -> Result<StreamResponse> {
        let mut attempt: usize = 0;
        loop {
            match self
                .inner
                .stream(
                    messages,
                    system,
                    system_suffix,
                    tools,
                    tool_instances,
                    config,
                )
                .await
            {
                Ok(r) => return Ok(r),
                Err(e) if attempt < self.max_retries && is_retryable(&e) => {
                    let input = messages.iter().map(Message::estimate_tokens).sum::<usize>()
                        + dyson_core::message::estimate_text_tokens(system)
                        + dyson_core::message::estimate_text_tokens(system_suffix)
                        + tools
                            .iter()
                            .map(|t| dyson_core::message::estimate_json_tokens(&t.input_schema))
                            .sum::<usize>();
                    crate::agent::task::budget::charge_uncertain_retry(
                        &config.model,
                        input as u64,
                        config.max_tokens,
                    )?;
                    // Server hint (Retry-After / X-RateLimit-Reset) wins over
                    // exponential backoff when present, but stays bounded so
                    // a misbehaving header can't hang the loop indefinitely.
                    let hint = match &e {
                        DysonError::LlmRateLimit { retry_after, .. }
                        | DysonError::LlmOverloaded { retry_after, .. } => *retry_after,
                        _ => None,
                    };
                    let backoff_ms = crate::util::backoff_with_jitter(self.base_delay_ms, attempt);
                    const MAX_HINT_MS: u64 = 90_000;
                    let delay_ms = match hint {
                        Some(d) => (d.as_millis() as u64).min(MAX_HINT_MS).max(backoff_ms),
                        None => backoff_ms,
                    };
                    tracing::warn!(
                        attempt = attempt + 1,
                        max = self.max_retries,
                        delay_ms,
                        honoring_server_hint = hint.is_some(),
                        error = %e,
                        "LLM call failed — backing off before retry"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Concurrency-limited decorator — caps in-flight requests per provider.
//
// Multiple controllers (telegram, http, swarm) and background agents
// (dreams, reflection, learning synthesis) all funnel through one provider
// client.  Without a cap they race into the same per-minute window and
// thunder-herd the rate limiter.  The semaphore sits OUTSIDE retry, so
// permits stay held across backoff sleeps — that serialises the sticky
// 429 case instead of letting every waiter retry independently and
// re-trip the limit.
// ---------------------------------------------------------------------------

pub(crate) struct ConcurrencyLimitedLlmClient {
    inner: Box<dyn LlmClient>,
    sem: std::sync::Arc<tokio::sync::Semaphore>,
}

struct ConcurrencyPermitStream {
    inner: Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Stream for ConcurrencyPermitStream {
    type Item = Result<StreamEvent>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.get_mut().inner.as_mut().poll_next(cx)
    }
}

impl ConcurrencyLimitedLlmClient {
    pub fn new(inner: Box<dyn LlmClient>, max_concurrent: usize) -> Self {
        Self {
            inner,
            sem: std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrent)),
        }
    }
}

#[async_trait]
impl LlmClient for ConcurrencyLimitedLlmClient {
    async fn stream(
        &self,
        messages: &[Message],
        system: &str,
        system_suffix: &str,
        tools: &[ToolDefinition],
        tool_instances: &std::collections::HashMap<String, std::sync::Arc<dyn Tool>>,
        config: &CompletionConfig,
    ) -> Result<StreamResponse> {
        // Acquire before issuing the request, then move the permit into the
        // returned stream so the cap covers retries, response headers, and
        // the complete streamed generation (including early consumer drop).
        let permit = self
            .sem
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore is never closed");
        let mut response = self
            .inner
            .stream(
                messages,
                system,
                system_suffix,
                tools,
                tool_instances,
                config,
            )
            .await?;
        response.stream = Box::pin(ConcurrencyPermitStream {
            inner: response.stream,
            _permit: permit,
        });
        Ok(response)
    }
}
