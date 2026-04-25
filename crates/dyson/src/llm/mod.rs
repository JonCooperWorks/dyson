// ===========================================================================
// LLM client — the provider-agnostic interface to language models.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Defines the `LlmClient` trait and supporting types (`CompletionConfig`,
//   `ToolDefinition`) that abstract over LLM providers.  The agent loop
//   calls `client.stream(...)` without knowing whether it's talking to
//   Anthropic, OpenAI, or a local model.
//
// Module layout:
//   mod.rs          — LlmClient trait, CompletionConfig, ToolDefinition,
//                     shared utilities (format_prompt, resolve_binary_path)
//   stream.rs       — StreamEvent and StopReason enums
//   anthropic.rs    — Anthropic Messages API implementation
//   openai.rs       — OpenAI Chat Completions API (pure protocol)
//   openai_compat.rs — OpenAI-compatible wrapper with dialect support
//   dialects/       — Model-specific adaptations (e.g., Gemma text tool calls)
//   claude_code.rs  — Claude Code CLI subprocess (no API key needed)
//   codex.rs        — Codex CLI subprocess (no API key needed)
//
// Why a trait?
//   Dyson is designed to support multiple LLM providers.  The trait boundary
//   means you can add OpenAI, Ollama, or any other provider by implementing
//   one trait.  The agent loop, skills, tools, and UI are completely
//   unaffected.
//
// Why streaming returns a Stream, not a callback?
//   Rust's `Stream` trait (from futures) composes naturally with async/await.
//   The stream handler can `while let Some(event) = stream.next().await`
//   and process events one by one.  This is more ergonomic and testable
//   than callbacks.  For testing, you can create a stream from a Vec of
//   events.
// ===========================================================================

pub mod anthropic;
pub mod claude_code;
pub(crate) mod cli_subprocess;
pub mod codex;
pub mod dialects;
pub mod gemini;
pub mod ollama_cloud;
pub mod openai;
pub mod openai_compat;
pub mod openrouter;
pub mod pricing;
pub mod registry;
pub(crate) mod sse_parser;
pub mod stream;



use std::pin::Pin;

use async_trait::async_trait;
use tokio_stream::Stream;

use std::fmt::Write as _;

use crate::error::{DysonError, Result};
use crate::llm::stream::StreamEvent;
use crate::message::{ContentBlock, Message, Role};
use crate::tool::Tool;

// ---------------------------------------------------------------------------
// CompletionConfig
// ---------------------------------------------------------------------------

/// Per-request configuration for an LLM completion.
///
/// Passed to [`LlmClient::stream()`] on each call.  The agent builds this
/// from [`AgentSettings`](crate::config::AgentSettings) at startup but can
/// adjust per-turn (e.g., lower temperature for tool-heavy tasks).
#[derive(Debug, Clone)]
pub struct CompletionConfig {
    /// Model identifier (e.g., "claude-sonnet-4-20250514").
    pub model: String,

    /// Maximum tokens to generate in this response.
    pub max_tokens: u32,

    /// Sampling temperature.  `None` means use the provider's default.
    ///
    /// Lower = more deterministic, higher = more creative.
    /// Tool-heavy tasks often benefit from lower temperature (0.0–0.3).
    pub temperature: Option<f64>,

    /// Provider-native tool entries to inject into the API request body.
    ///
    /// Used by the advisor pattern to inject `advisor_20260301` entries into
    /// Anthropic requests.  Non-Anthropic clients ignore this field.
    pub api_tool_injections: Vec<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// ToolDefinition
// ---------------------------------------------------------------------------

/// A tool definition sent to the LLM so it knows what tools are available.
///
/// Built from [`Tool::name()`], [`Tool::description()`], and
/// [`Tool::input_schema()`] at agent startup.  Sent as part of every
/// LLM request.
#[derive(Debug, Clone)]
pub struct ToolDefinition {
    /// Tool name (must match what the LLM sends back in tool_use blocks).
    pub name: String,

    /// Human-readable description of what the tool does.
    pub description: String,

    /// JSON Schema for the tool's input parameters.
    pub input_schema: serde_json::Value,

    /// When `true`, this tool is only sent to providers that execute tools
    /// directly (ToolMode::Execute).  Providers with their own agent loop
    /// (Claude Code, Codex) will not see this tool.
    pub agent_only: bool,
}


// ---------------------------------------------------------------------------
// ToolMode — how the agent loop should handle tool calls from this stream.
// ---------------------------------------------------------------------------

/// Controls whether the agent loop executes tool calls from the stream.
///
/// This replaces the old `handles_tools_internally()` boolean.  Instead of
/// a flag on the trait, the mode travels with the `StreamResponse` — the
/// information is co-located with the data it describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolMode {
    /// Dyson executes tool calls itself (standard behavior).
    ///
    /// The agent loop dispatches tool calls through the sandbox, runs them
    /// concurrently, and feeds results back to the LLM.
    Execute,

    /// The provider already executed tool calls internally.
    ///
    /// ToolUse stream events are informational only (displayed to the user
    /// but not re-executed).  The agent loop breaks after one iteration.
    /// Used by Claude Code and Codex, which run their own agent loops.
    Observe,
}

// ---------------------------------------------------------------------------
// StreamResponse — what stream() returns.
// ---------------------------------------------------------------------------

/// The result of starting a streaming LLM completion.
///
/// Bundles the event stream with its [`ToolMode`] so the agent loop knows
/// how to handle tool calls without querying the client separately.
pub struct StreamResponse {
    /// The event stream to consume.
    pub stream: Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>,

    /// How the agent loop should handle tool calls in this stream.
    pub tool_mode: ToolMode,

    /// Input token count for this request (if known at request time).
    ///
    /// Some providers report input tokens in the response headers or
    /// initial event.  `None` if not available until the stream completes.
    pub input_tokens: Option<usize>,
}

// ---------------------------------------------------------------------------
// LlmClient trait
// ---------------------------------------------------------------------------

/// Provider-agnostic interface for streaming LLM completions.
///
/// Each provider (Anthropic, OpenAI, local) implements this trait.  The
/// agent loop calls `stream()` and consumes the resulting `StreamResponse`
/// without knowing anything about the underlying API.
///
/// ## Why async?
///
/// LLM calls are network I/O — building the HTTP request, streaming the
/// response.  Async lets the tokio runtime do other work (handle Ctrl-C,
/// run the UI) while waiting for the first token.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Start a streaming completion.
    ///
    /// ## Parameters
    ///
    /// - `messages`: The conversation history (user messages, assistant
    ///   responses, tool results).
    /// - `system`: The system prompt (passed separately, not as a message).
    ///   This is the **stable** prefix that doesn't change between turns
    ///   within a session — ideal for KV cache / prompt caching.
    /// - `system_suffix`: Ephemeral context appended after the stable system
    ///   prompt (e.g. current timestamp, per-turn skill fragments).  Changes
    ///   every turn, so providers should NOT cache this part.  Pass `""` when
    ///   there is nothing to append.
    /// - `tools`: Available tool definitions (the LLM decides which to use).
    /// - `config`: Model, max_tokens, temperature.
    ///
    /// ## Returns
    ///
    /// A [`StreamResponse`] containing the event stream and its
    /// [`ToolMode`].  The stream ends with `StreamEvent::MessageComplete`.
    async fn stream(
        &self,
        messages: &[Message],
        system: &str,
        system_suffix: &str,
        tools: &[ToolDefinition],
        config: &CompletionConfig,
    ) -> Result<StreamResponse>;

    /// Pass Dyson's tools to CLI-based backends for MCP exposure.
    /// API-based clients (Anthropic, OpenAI) ignore this — no-op default.
    fn set_mcp_tools(&self, _tools: std::collections::HashMap<String, std::sync::Arc<dyn Tool>>) {}
}

// ---------------------------------------------------------------------------
// Client factory
// ---------------------------------------------------------------------------

/// Create an LLM client from agent settings.
///
/// Factory function that dispatches on `settings.provider` to construct
/// the appropriate client implementation.  Used by controllers to create
/// a client per session/message without duplicating provider-matching logic.
///
/// ## Parameters
///
/// - `settings`: Agent configuration (provider, model, API key, base URL).
/// - `workspace`: Shared workspace reference, used by `ClaudeCodeClient` and
///   `CodexClient`.  When `Some`, the client starts an in-process HTTP MCP server
///   that exposes workspace tools (view, search, update) to the CLI subprocess.
///   Ignored by Anthropic and OpenAI clients (they use Dyson's own tool system).
/// - `dangerous_no_sandbox`: Whether `--dangerous-no-sandbox` was passed on
///   the CLI.  Forwarded to `ClaudeCodeClient` → `McpHttpServer` for future
///   sandbox enforcement of MCP tool calls.  No effect on Anthropic/OpenAI
///   clients (their sandbox is applied by the agent loop, not the LLM client).
///
/// ## Provider behavior
///
/// | Provider       | workspace used? | sandbox flag used? | Tools |
/// |----------------|-----------------|-------------------|-------|
/// | `Anthropic`    | No              | No                | Dyson's tool system |
/// | `OpenAi`       | No              | No                | Dyson's tool system |
/// | `OpenRouter`   | No              | No                | Dyson's tool system |
/// | `ClaudeCode`   | Yes (MCP server)| Yes (forwarded)   | Claude Code built-in + workspace via MCP |
/// | `Codex`        | Yes (MCP server)| Yes (forwarded)   | Codex built-in + workspace via MCP |
///
/// ## Why workspace is passed here (not at stream time)
///
/// The workspace Arc is part of the client's configuration, not per-request
/// state.  All LLM turns within a session share the same workspace.  Passing
/// it at construction time simplifies the `LlmClient` trait (stream() doesn't
/// need workspace-awareness) and keeps the workspace coupling isolated to
/// the CLI-subprocess backends (Claude Code and Codex).
pub(crate) fn create_client(
    settings: &crate::config::AgentSettings,
    workspace: Option<crate::workspace::WorkspaceHandle>,
    dangerous_no_sandbox: bool,
) -> Box<dyn LlmClient> {
    let entry = registry::lookup(&settings.provider);
    let config = registry::ClientConfig {
        api_key: settings.api_key.expose(),
        base_url: settings.base_url.as_deref(),
        workspace,
        dangerous_no_sandbox,
    };
    let inner = (entry.create_client)(&config);
    let retrying: Box<dyn LlmClient> =
        Box::new(RetryingLlmClient::new(inner, settings.max_retries));
    // Cap of 0 disables the wrapper (preserves pre-existing unbounded
    // behaviour for anyone who explicitly opts out).
    if settings.max_concurrent_llm_calls == 0 {
        retrying
    } else {
        Box::new(ConcurrencyLimitedLlmClient::new(
            retrying,
            settings.max_concurrent_llm_calls,
        ))
    }
}

// ---------------------------------------------------------------------------
// Shared SSE parser limits
// ---------------------------------------------------------------------------

/// Maximum SSE line buffer size before aborting the stream.
///
/// Protects against malformed streams that never send newlines.  Shared
/// across all SSE-based LLM clients (Anthropic, OpenAI).
pub(crate) const MAX_LINE_BUFFER: usize = 10 * 1024 * 1024; // 10 MB

/// Maximum accumulated JSON for a single tool call's input.
///
/// Prevents a runaway stream from consuming unbounded memory during
/// tool argument accumulation.
pub(crate) const MAX_TOOL_JSON: usize = 10 * 1024 * 1024; // 10 MB

/// Maximum accumulated JSON across *all* concurrent tool call buffers in a
/// single stream.  Without this, MAX_TOOL_JSON × MAX_ACTIVE_TOOL_BUFFERS
/// allows ~1 GB of buffered tool input per stream.  50 MB is generous for
/// well-behaved streams and catches pathological models before the host OOMs.
pub(crate) const MAX_TOTAL_TOOL_JSON: usize = 50 * 1024 * 1024; // 50 MB

/// Maximum number of concurrent tool call buffers the SSE parser will
/// track.  A well-behaved stream will have at most a handful; this cap
/// protects against malformed streams that never close content blocks.
pub(crate) const MAX_ACTIVE_TOOL_BUFFERS: usize = 100;

// ---------------------------------------------------------------------------
// Shared tool call buffer
// ---------------------------------------------------------------------------

/// Accumulation buffer for a single in-progress tool call during streaming.
///
/// All LLM providers stream tool call arguments as partial JSON fragments.
/// This struct collects those fragments until the tool call is complete,
/// at which point [`finalize_tool_call`] parses the accumulated JSON.
pub(crate) struct ToolCallBuffer {
    /// Unique identifier for this tool call.
    pub id: String,
    /// Name of the tool being called.
    pub name: String,
    /// Accumulated partial JSON fragments.
    pub json: String,
}

/// Parse the accumulated JSON in a [`ToolCallBuffer`] and produce a
/// `ToolUseComplete` event.
///
/// If JSON parsing fails, emits a `ToolUseComplete` with an `_parse_error`
/// field so the tool call is still dispatched (preserving the agent loop's
/// tool_result contract with the LLM) but the error is visible.
pub(crate) fn finalize_tool_call(buf: ToolCallBuffer) -> Result<StreamEvent> {
    let input = match serde_json::from_str(&buf.json) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(
                tool = buf.name,
                json = buf.json,
                error = %e,
                "failed to parse accumulated tool call JSON — tool will receive error input"
            );
            // Include the parse error so the tool (or agent loop) can
            // surface a clear error message instead of a cryptic
            // "missing required field" from an empty object.
            serde_json::json!({
                "_parse_error": format!("malformed tool input JSON: {e}"),
                "_raw_json": buf.json,
            })
        }
    };

    Ok(StreamEvent::ToolUseComplete {
        id: buf.id,
        name: buf.name,
        input,
    })
}

// ---------------------------------------------------------------------------
// Shared SSE line buffer — deduplicates parsing logic across providers.
// ---------------------------------------------------------------------------

/// Shared line-buffering state for SSE parsers.
///
/// Both Anthropic and OpenAI SSE parsers need identical logic for:
/// - Buffering incomplete lines from chunked byte streams
/// - Splitting on newlines to yield complete data payloads
/// - Guarding against unbounded buffer growth
///
/// This struct extracts that shared concern so each provider's parser
/// only implements the provider-specific JSON → StreamEvent mapping.
pub(crate) struct SseLineBuffer {
    /// Buffer for incomplete lines (raw bytes received but no newline yet).
    ///
    /// We store raw bytes instead of `String` so that multi-byte UTF-8
    /// characters split across chunk boundaries are preserved correctly.
    /// Decoding to UTF-8 happens only when a complete line is extracted.
    buffer: Vec<u8>,
}

impl SseLineBuffer {
    pub const fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Feed raw bytes and return complete `data:` payloads.
    ///
    /// Handles line buffering, SSE protocol (skips comments, event: lines),
    /// and the `[DONE]` sentinel.  Returns `Err` if the buffer exceeds
    /// `MAX_LINE_BUFFER`.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        if self.buffer.len() + bytes.len() > MAX_LINE_BUFFER {
            return Err(crate::error::DysonError::Llm(
                "SSE line buffer exceeded 10 MB — aborting stream".into(),
            ));
        }
        self.buffer.extend_from_slice(bytes);

        let mut payloads = Vec::new();

        while let Some(newline_pos) = self.buffer.iter().position(|&b| b == b'\n') {
            // Decode the line in-place from the buffer without allocating a
            // separate Vec — just borrow the slice and drain afterward.
            let line = std::str::from_utf8(&self.buffer[..newline_pos])
                .map(str::trim)
                .unwrap_or_else(|_| {
                    // Fallback for invalid UTF-8: use lossy conversion.
                    // This path is rare for SSE streams.
                    ""
                });

            // Skip empty lines, comments, and event: lines.
            if !(line.is_empty() || line.starts_with(':') || line.starts_with("event:"))
                && let Some(data) = line.strip_prefix("data:")
            {
                payloads.push(data.trim().to_string());
            }

            // Drain the line including the newline byte.
            self.buffer.drain(..=newline_pos);
        }

        Ok(payloads)
    }
}

// ---------------------------------------------------------------------------
// Shared SSE stream creation — deduplicates streaming logic across providers.
// ---------------------------------------------------------------------------

/// Trait for SSE parsers that can consume raw bytes and produce stream events.
///
/// Both `SseParser` (Anthropic) and `OpenAiSseParser` implement this,
/// enabling shared stream creation via [`sse_event_stream`].
pub(crate) trait SseStreamParser {
    fn feed(&mut self, bytes: &[u8]) -> Vec<Result<StreamEvent>>;
}

/// Create a stream of `StreamEvent`s from an HTTP byte stream and an SSE parser.
///
/// This is the shared streaming core for all SSE-based LLM providers.
/// Each provider creates its own parser type, but the streaming boilerplate
/// (byte buffering, error mapping, event yielding) is identical.
pub(crate) fn sse_event_stream<P: SseStreamParser + Send + 'static>(
    response: reqwest::Response,
    mut parser: P,
) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
    Box::pin(async_stream::stream! {
        use tokio_stream::StreamExt as _;
        let byte_stream = response.bytes_stream();
        tokio::pin!(byte_stream);

        while let Some(chunk_result) = byte_stream.next().await {
            match chunk_result {
                Ok(bytes) => {
                    for event in parser.feed(&bytes) {
                        yield event;
                    }
                }
                Err(e) => {
                    yield Err(DysonError::Http(e));
                }
            }
        }
    })
}

/// Parse a wait hint out of rate-limit response headers.
///
/// Looks at `Retry-After` first (RFC 7231 — integer seconds; HTTP-date form
/// is rare in API responses and intentionally not handled to avoid pulling
/// in a date-parsing dep), then `X-RateLimit-Reset` (Unix timestamp in
/// seconds, occasionally milliseconds — disambiguated by magnitude).
///
/// Returns the duration to wait from now, or `None` if no usable hint is
/// present.
pub(crate) fn parse_retry_hint(
    headers: &reqwest::header::HeaderMap,
) -> Option<std::time::Duration> {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    if let Some(v) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        && let Ok(secs) = v.trim().parse::<u64>()
    {
        return Some(Duration::from_secs(secs));
    }

    if let Some(v) = headers
        .get("x-ratelimit-reset")
        .and_then(|h| h.to_str().ok())
        && let Ok(ts) = v.trim().parse::<u64>()
    {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        // OpenRouter has used both seconds and ms over time.  Anything past
        // year 2100 in seconds (4_102_444_800) must be ms.
        let target_secs = if ts > 4_102_444_800 { ts / 1000 } else { ts };
        if target_secs > now {
            return Some(Duration::from_secs(target_secs - now));
        }
    }

    None
}

/// Map an HTTP error response to a `DysonError`.
///
/// Shared by all API-based LLM providers (Anthropic, OpenAI, Gemini).
/// Reads the response body and maps status codes to error variants:
/// - 429 → `LlmRateLimit` (with `retry_after` from response headers)
/// - 502/503/529 → `LlmOverloaded` (with `retry_after` from response headers)
/// - Everything else → `Llm`
pub(crate) async fn map_http_error(
    response: reqwest::Response,
    provider: &str,
) -> DysonError {
    let status = response.status();
    // Pull headers off before consuming the response into text.
    let retry_after = parse_retry_hint(response.headers());
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "failed to read error body".into());

    match status.as_u16() {
        429 => DysonError::LlmRateLimit {
            message: format!("{provider} API rate limited: {body}"),
            retry_after,
        },
        502 | 503 | 529 => DysonError::LlmOverloaded {
            message: format!("{provider} API returned {status}: {body}"),
            retry_after,
        },
        _ => DysonError::Llm(format!("{provider} API returned {status}: {body}")),
    }
}

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
    fn with_base_delay(
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
        config: &CompletionConfig,
    ) -> Result<StreamResponse> {
        let mut attempt: usize = 0;
        loop {
            match self
                .inner
                .stream(messages, system, system_suffix, tools, config)
                .await
            {
                Ok(r) => return Ok(r),
                Err(e) if attempt < self.max_retries && is_retryable(&e) => {
                    // Server hint (Retry-After / X-RateLimit-Reset) wins over
                    // exponential backoff when present, but stays bounded so
                    // a misbehaving header can't hang the loop indefinitely.
                    let hint = match &e {
                        DysonError::LlmRateLimit { retry_after, .. }
                        | DysonError::LlmOverloaded { retry_after, .. } => *retry_after,
                        _ => None,
                    };
                    let exp_ms = self
                        .base_delay_ms
                        .saturating_mul(1u64 << attempt.min(6));
                    let jitter_ms = rand::random::<u64>() % (exp_ms / 2 + 1);
                    let backoff_ms = exp_ms + jitter_ms;
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

    fn set_mcp_tools(&self, tools: std::collections::HashMap<String, std::sync::Arc<dyn Tool>>) {
        self.inner.set_mcp_tools(tools);
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
        config: &CompletionConfig,
    ) -> Result<StreamResponse> {
        // Acquire a permit before issuing the request.  Permit drops when
        // _permit goes out of scope — we hold it through the whole
        // .stream() call (including any retries inside the inner client).
        let _permit = self
            .sem
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore is never closed");
        self.inner
            .stream(messages, system, system_suffix, tools, config)
            .await
    }

    fn set_mcp_tools(&self, tools: std::collections::HashMap<String, std::sync::Arc<dyn Tool>>) {
        self.inner.set_mcp_tools(tools);
    }
}

/// Build a `StreamResponse` from an HTTP response and SSE parser.
///
/// Shared by all SSE-based LLM providers.  Each provider just passes its
/// own parser type — the wrapping is identical.
pub(crate) fn build_stream_response<P: SseStreamParser + Send + 'static>(
    response: reqwest::Response,
    parser: P,
) -> StreamResponse {
    StreamResponse {
        stream: sse_event_stream(response, parser),
        tool_mode: ToolMode::Execute,
        input_tokens: None,
    }
}

/// Concatenate a stable system prompt with an ephemeral per-turn suffix.
///
/// Returns the stable prompt as-is when the suffix is empty, avoiding
/// a needless allocation.  Used by providers that pass the system prompt
/// as a single string (OpenAI, Gemini).  Anthropic handles this
/// differently with separate cache-control blocks.
pub(crate) fn concat_system_prompt(system: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        system.to_string()
    } else {
        format!("{system}\n\n{suffix}")
    }
}

// ---------------------------------------------------------------------------
// Shared utilities for CLI-subprocess-based clients
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Shared MCP server startup for CLI-subprocess clients
// ---------------------------------------------------------------------------

/// Info about a running MCP HTTP server started for a CLI subprocess.
pub(crate) struct McpServerInfo {
    /// The port the server is listening on (127.0.0.1).
    pub port: u16,
    /// The server task handle — drop to stop the server.
    pub handle: tokio::task::JoinHandle<()>,
    /// Bearer token for authenticating requests.
    pub token: String,
    /// The base URL for the MCP endpoint (e.g., "http://127.0.0.1:{port}/mcp").
    pub url: String,
}

/// Start an MCP server exposing workspace + Dyson tools to CLI subprocesses.
/// Fresh server per `stream()` call; lives until the returned handle is dropped.
pub(crate) async fn start_mcp_server(
    workspace: &crate::workspace::WorkspaceHandle,
    extra_tools: std::collections::HashMap<String, std::sync::Arc<dyn Tool>>,
) -> Result<McpServerInfo> {
    use crate::skill::mcp::serve::McpHttpServer;
    use std::sync::Arc;

    let server = Arc::new(McpHttpServer::new(Arc::clone(workspace), extra_tools));

    let (port, handle, token) = server.start().await.map_err(|e| {
        crate::error::DysonError::Llm(format!("failed to start MCP HTTP server: {e}"))
    })?;

    let url = format!("http://127.0.0.1:{port}/mcp");

    Ok(McpServerInfo {
        port,
        handle,
        token,
        url,
    })
}

/// Resolve the absolute path to a CLI binary by name.
///
/// Uses `which <name>` to find it on the current PATH.  This is important
/// for service environments (systemd, launchd) where PATH is minimal and
/// won't include npm global bin directories.  By resolving at startup
/// (which happens before daemonizing or during the first run), we capture
/// the full path while the user's PATH is still available.
///
/// Falls back to the bare binary name if `which` fails (lets the OS search
/// PATH at spawn time).
pub(crate) fn resolve_binary_path(name: &str) -> String {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    tracing::info!(binary = name, path = path, "resolved binary path");
                    return Some(path);
                }
            }
            None
        })
        .unwrap_or_else(|| {
            tracing::warn!(
                binary = name,
                "could not resolve path — falling back to bare name"
            );
            name.to_string()
        })
}

/// Format a conversation history and tool definitions into a single text
/// prompt for CLI-subprocess-based clients.
///
/// CLI agents like `claude -p` and `codex exec` take a single text prompt
/// rather than structured message arrays.  This function converts the
/// conversation history into a readable text format.
///
/// For single-turn conversations (the common case), the prompt is just
/// the user's latest message.  For multi-turn conversations with tool
/// results, the full history is included so the model has context.
///
/// ## Format
///
/// ```text
/// [Previous conversation:]
///
/// User: What files are here?
///
/// Assistant: Let me check.
/// [Used tool: bash with input: {"command":"ls"}]
///
/// Tool result (bash): Cargo.toml
/// src/
///
/// Assistant: There are 2 items.
///
/// [Current message:]
///
/// Tell me more about Cargo.toml
/// ```
pub(crate) fn format_prompt(messages: &[Message], tools: &[&ToolDefinition]) -> String {
    // Single user message with no history and no tools — just return the text.
    if messages.len() == 1
        && tools.is_empty()
        && let Some(ContentBlock::Text { text }) = messages[0].content.first()
    {
        return text.clone();
    }

    let mut prompt = String::new();

    // Multi-turn: format the history.
    if messages.len() > 1 {
        prompt.push_str("[Previous conversation:]\n\n");

        for (i, msg) in messages.iter().enumerate() {
            // Skip the last message — we'll add it separately below.
            if i == messages.len() - 1 {
                break;
            }

            let role_label = match msg.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
            };

            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        let _ = write!(prompt, "{role_label}: {text}\n\n");
                    }
                    ContentBlock::ToolUse { name, input, .. } => {
                        let _ = write!(prompt, "[Used tool: {name} with input: {input}]\n\n");
                    }
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        let label = if *is_error {
                            "Tool error"
                        } else {
                            "Tool result"
                        };
                        let _ = write!(prompt, "{label}: {content}\n\n");
                    }
                    ContentBlock::Image { .. } => {
                        prompt.push_str("[Image attached]\n\n");
                    }
                    ContentBlock::Document { extracted_text, .. } => {
                        let _ = write!(prompt, "[PDF document]\n{extracted_text}\n\n");
                    }
                    ContentBlock::Thinking { .. } => {}
                    ContentBlock::Artefact { .. } => {}
                }
            }
        }

        prompt.push_str("[Current message:]\n\n");
    }

    // Add the last (current) message.
    if let Some(msg) = messages.last() {
        for block in &msg.content {
            if let ContentBlock::Text { text } = block {
                prompt.push_str(text);
            }
        }
    }

    // Append tool definitions if any.
    if !tools.is_empty() {
        prompt.push_str("\n\n[Available tools:]\n");
        for tool in tools {
            let _ = write!(
                prompt,
                "\n- **{}**: {}\n  Input schema: {}\n",
                tool.name, tool.description, tool.input_schema
            );
        }
    }

    prompt
}

// ===========================================================================
// Tests for shared utilities
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // format_prompt tests
    // -----------------------------------------------------------------------

    #[test]
    fn single_message_is_just_text() {
        let messages = vec![Message::user("hello")];
        let prompt = format_prompt(&messages, &[]);
        assert_eq!(prompt, "hello");
    }

    #[test]
    fn multi_turn_includes_history() {
        let messages = vec![
            Message::user("what files?"),
            Message::assistant(vec![ContentBlock::Text {
                text: "Let me check.".into(),
            }]),
            Message::user("thanks"),
        ];
        let prompt = format_prompt(&messages, &[]);
        assert!(prompt.contains("[Previous conversation:]"));
        assert!(prompt.contains("User: what files?"));
        assert!(prompt.contains("Assistant: Let me check."));
        assert!(prompt.contains("[Current message:]"));
        assert!(prompt.contains("thanks"));
    }

    #[test]
    fn tool_results_in_history() {
        let messages = vec![
            Message::user("list files"),
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }]),
            Message::tool_result("call_1", "Cargo.toml\nsrc/", false),
            Message::user("nice"),
        ];
        let prompt = format_prompt(&messages, &[]);
        assert!(prompt.contains("[Used tool: bash"));
        assert!(prompt.contains("Tool result: Cargo.toml"));
    }

    #[test]
    fn tools_appended_to_prompt() {
        let messages = vec![Message::user("help")];
        let tool = ToolDefinition {
            name: "bash".into(),
            description: "Run commands".into(),
            input_schema: serde_json::json!({"type": "object"}),
            agent_only: false,
        };
        let prompt = format_prompt(&messages, &[&tool]);
        assert!(prompt.contains("[Available tools:]"));
        assert!(prompt.contains("**bash**"));
    }

    // -----------------------------------------------------------------------
    // SseLineBuffer tests
    // -----------------------------------------------------------------------

    #[test]
    fn sse_line_buffer_extracts_data_payloads() {
        let mut buf = SseLineBuffer::new();
        let payloads = buf.feed(b"data: {\"type\":\"text\"}\n\n").unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0], "{\"type\":\"text\"}");
    }

    #[test]
    fn sse_line_buffer_skips_comments_and_events() {
        let mut buf = SseLineBuffer::new();
        let payloads = buf
            .feed(b": this is a comment\nevent: message_start\ndata: {\"ok\":true}\n\n")
            .unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0], "{\"ok\":true}");
    }

    #[test]
    fn sse_line_buffer_handles_partial_lines() {
        let mut buf = SseLineBuffer::new();

        // First chunk: incomplete line.
        let p1 = buf.feed(b"data: {\"part").unwrap();
        assert!(p1.is_empty());

        // Second chunk: completes the line.
        let p2 = buf.feed(b"ial\":true}\n\n").unwrap();
        assert_eq!(p2.len(), 1);
        assert_eq!(p2[0], "{\"partial\":true}");
    }

    #[test]
    fn sse_line_buffer_returns_done_sentinel() {
        let mut buf = SseLineBuffer::new();
        let payloads = buf.feed(b"data: [DONE]\n\n").unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0], "[DONE]");
    }

    #[test]
    fn sse_line_buffer_preserves_multibyte_utf8_across_chunks() {
        let mut buf = SseLineBuffer::new();

        // "├──" is three box-drawing characters, each 3 bytes in UTF-8.
        // U+251C (├) = E2 94 9C, U+2500 (─) = E2 94 80
        // Split the data line so the chunk boundary falls mid-character.
        let full = "data: {\"text\":\"├──\"}\n";
        let bytes = full.as_bytes();

        // Split at byte 17: bytes 15-17 are E2 94 9C (├).
        // split_at(17) puts [..17] (ending with 0x94) in chunk1, and
        // [17..] (starting with 0x9C) in chunk2, splitting mid-character.
        let (chunk1, chunk2) = bytes.split_at(17);
        assert!(
            chunk1.last().copied() == Some(0x94),
            "sanity: split should land mid-character"
        );

        let p1 = buf.feed(chunk1).unwrap();
        assert!(p1.is_empty(), "first chunk has no complete line");

        let p2 = buf.feed(chunk2).unwrap();
        assert_eq!(p2.len(), 1);
        assert_eq!(p2[0], "{\"text\":\"├──\"}");
    }

    #[test]
    fn sse_line_buffer_rejects_oversized_input() {
        let mut buf = SseLineBuffer::new();
        let chunk = vec![b'x'; 10 * 1024 * 1024 + 1];
        let result = buf.feed(&chunk);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("10 MB"));
    }

    // -----------------------------------------------------------------------
    // finalize_tool_call tests
    // -----------------------------------------------------------------------

    #[test]
    fn finalize_valid_json() {
        let buf = ToolCallBuffer {
            id: "call_1".into(),
            name: "bash".into(),
            json: r#"{"command":"ls"}"#.into(),
        };
        let event = finalize_tool_call(buf).unwrap();
        match event {
            stream::StreamEvent::ToolUseComplete { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "ls");
            }
            other => panic!("expected ToolUseComplete, got: {other:?}"),
        }
    }

    #[test]
    fn finalize_malformed_json_includes_parse_error() {
        let buf = ToolCallBuffer {
            id: "call_2".into(),
            name: "bash".into(),
            json: "{broken json".into(),
        };
        let event = finalize_tool_call(buf).unwrap();
        match event {
            stream::StreamEvent::ToolUseComplete { input, .. } => {
                // Should contain the parse error info, not an empty object.
                assert!(
                    input["_parse_error"]
                        .as_str()
                        .unwrap()
                        .contains("malformed")
                );
                assert_eq!(input["_raw_json"], "{broken json");
            }
            other => panic!("expected ToolUseComplete, got: {other:?}"),
        }
    }

    #[test]
    fn finalize_empty_json_includes_parse_error() {
        let buf = ToolCallBuffer {
            id: "call_3".into(),
            name: "bash".into(),
            json: String::new(),
        };
        let event = finalize_tool_call(buf).unwrap();
        match event {
            stream::StreamEvent::ToolUseComplete { input, .. } => {
                assert!(input.get("_parse_error").is_some());
            }
            other => panic!("expected ToolUseComplete, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // RetryingLlmClient tests
    // -----------------------------------------------------------------------

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Test-only client that returns a prepared sequence of `Result`s on each
    /// `stream()` call.  Counts attempts so tests can assert retry behavior.
    /// `attempts` is an `Arc<AtomicUsize>` so the test can observe the count
    /// after the client is moved into `RetryingLlmClient`.
    struct ScriptedClient {
        results: std::sync::Mutex<Vec<Result<()>>>,
        attempts: Arc<AtomicUsize>,
    }

    impl ScriptedClient {
        fn new(results: Vec<Result<()>>) -> (Self, Arc<AtomicUsize>) {
            let attempts = Arc::new(AtomicUsize::new(0));
            let client = Self {
                results: std::sync::Mutex::new(results),
                attempts: Arc::clone(&attempts),
            };
            (client, attempts)
        }
    }

    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn stream(
            &self,
            _messages: &[Message],
            _system: &str,
            _system_suffix: &str,
            _tools: &[ToolDefinition],
            _config: &CompletionConfig,
        ) -> Result<StreamResponse> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let next = self.results.lock().unwrap().remove(0);
            next.map(|()| StreamResponse {
                stream: Box::pin(tokio_stream::iter(std::iter::empty())),
                tool_mode: ToolMode::Execute,
                input_tokens: None,
            })
        }
    }

    fn empty_config() -> CompletionConfig {
        CompletionConfig {
            model: "test".into(),
            max_tokens: 1,
            temperature: None,
            api_tool_injections: vec![],
        }
    }

    fn assert_err(r: Result<StreamResponse>) -> DysonError {
        match r {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(e) => e,
        }
    }

    fn rate_limit_err(retry_after: Option<std::time::Duration>) -> DysonError {
        DysonError::LlmRateLimit {
            message: "429".into(),
            retry_after,
        }
    }

    fn overloaded_err(retry_after: Option<std::time::Duration>) -> DysonError {
        DysonError::LlmOverloaded {
            message: "529".into(),
            retry_after,
        }
    }

    #[tokio::test]
    async fn retries_rate_limit_then_succeeds() {
        let (scripted, attempts) = ScriptedClient::new(vec![
            Err(rate_limit_err(None)),
            Err(rate_limit_err(None)),
            Ok(()),
        ]);
        let client = RetryingLlmClient::with_base_delay(Box::new(scripted), 3, 1);

        let result = client.stream(&[], "", "", &[], &empty_config()).await;
        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_max_retries() {
        let (scripted, attempts) = ScriptedClient::new(vec![
            Err(rate_limit_err(None)),
            Err(rate_limit_err(None)),
            Err(rate_limit_err(None)),
        ]);
        let client = RetryingLlmClient::with_base_delay(Box::new(scripted), 2, 1);

        let err = assert_err(
            client
                .stream(&[], "", "", &[], &empty_config())
                .await,
        );
        assert!(matches!(err, DysonError::LlmRateLimit { .. }));
        // initial + 2 retries = 3 attempts.
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn non_retryable_error_is_not_retried() {
        let (scripted, attempts) =
            ScriptedClient::new(vec![Err(DysonError::Llm("auth failed".into()))]);
        let client = RetryingLlmClient::with_base_delay(Box::new(scripted), 5, 1);

        let err = assert_err(
            client
                .stream(&[], "", "", &[], &empty_config())
                .await,
        );
        assert!(matches!(err, DysonError::Llm(_)));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn zero_max_retries_disables_retry() {
        let (scripted, attempts) = ScriptedClient::new(vec![Err(overloaded_err(None))]);
        let client = RetryingLlmClient::with_base_delay(Box::new(scripted), 0, 1);

        let err = assert_err(
            client
                .stream(&[], "", "", &[], &empty_config())
                .await,
        );
        assert!(matches!(err, DysonError::LlmOverloaded { .. }));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn parse_retry_hint_reads_retry_after_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
        let hint = parse_retry_hint(&headers).expect("expected a hint");
        assert_eq!(hint, std::time::Duration::from_secs(30));
    }

    #[test]
    fn parse_retry_hint_reads_x_ratelimit_reset_seconds() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let target = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 45;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-reset", target.to_string().parse().unwrap());
        let hint = parse_retry_hint(&headers).expect("expected a hint");
        // Allow 2s slack for clock drift between insertion and the reread.
        assert!(
            hint.as_secs() >= 43 && hint.as_secs() <= 45,
            "hint was {hint:?}"
        );
    }

    #[test]
    fn parse_retry_hint_handles_x_ratelimit_reset_milliseconds() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let target_ms = (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 60) * 1000;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-reset", target_ms.to_string().parse().unwrap());
        let hint = parse_retry_hint(&headers).expect("expected a hint");
        assert!(
            hint.as_secs() >= 58 && hint.as_secs() <= 60,
            "hint was {hint:?}"
        );
    }

    #[test]
    fn parse_retry_hint_returns_none_when_absent() {
        let headers = reqwest::header::HeaderMap::new();
        assert!(parse_retry_hint(&headers).is_none());
    }

    #[test]
    fn parse_retry_hint_prefers_retry_after_over_reset() {
        // When both headers are present, Retry-After wins.
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "5".parse().unwrap());
        headers.insert("x-ratelimit-reset", "9999999999".parse().unwrap());
        let hint = parse_retry_hint(&headers).expect("expected a hint");
        assert_eq!(hint, std::time::Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn retry_honors_server_hint_over_exponential() {
        // base_delay_ms=1 → exponential floor on first retry is ~1-2ms.
        // Hint of 5s must dominate.  Paused time auto-advances through
        // sleeps, so the assertion is on virtual elapsed time.
        let (scripted, _attempts) = ScriptedClient::new(vec![
            Err(rate_limit_err(Some(std::time::Duration::from_secs(5)))),
            Ok(()),
        ]);
        let client = RetryingLlmClient::with_base_delay(Box::new(scripted), 3, 1);
        let start = tokio::time::Instant::now();
        client
            .stream(&[], "", "", &[], &empty_config())
            .await
            .expect("retry should succeed");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_secs(5)
                && elapsed < std::time::Duration::from_secs(6),
            "expected ~5s wait honoring hint, got {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_caps_excessive_server_hint() {
        // A 10-minute server hint must be capped to MAX_HINT_MS (90s) so a
        // misbehaving header can't wedge the loop.  Tokio's paused-time mode
        // lets us assert the actual elapsed virtual time without sleeping.
        let (scripted, attempts) = ScriptedClient::new(vec![
            Err(rate_limit_err(Some(std::time::Duration::from_secs(600)))),
            Ok(()),
        ]);
        let client = RetryingLlmClient::with_base_delay(Box::new(scripted), 1, 1);
        let start = tokio::time::Instant::now();
        client
            .stream(&[], "", "", &[], &empty_config())
            .await
            .expect("retry should succeed");
        let elapsed = start.elapsed();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(
            elapsed <= std::time::Duration::from_millis(90_001),
            "expected wait capped at 90s, got {elapsed:?}"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(89_000),
            "expected wait near the 90s cap, got {elapsed:?}"
        );
    }

    // -----------------------------------------------------------------------
    // ConcurrencyLimitedLlmClient tests
    // -----------------------------------------------------------------------

    /// Test client that records how many `stream()` calls overlap.  Each
    /// call increments an in-flight counter, awaits a notification gating
    /// release, then decrements.  The peak counter shows whether the
    /// semaphore actually serialised callers.
    struct OverlapTrackingClient {
        in_flight: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        gate: Arc<tokio::sync::Notify>,
    }

    impl OverlapTrackingClient {
        fn new() -> (Self, Arc<AtomicUsize>, Arc<tokio::sync::Notify>) {
            let in_flight = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let gate = Arc::new(tokio::sync::Notify::new());
            (
                Self {
                    in_flight: Arc::clone(&in_flight),
                    peak: Arc::clone(&peak),
                    gate: Arc::clone(&gate),
                },
                peak,
                gate,
            )
        }
    }

    #[async_trait]
    impl LlmClient for OverlapTrackingClient {
        async fn stream(
            &self,
            _messages: &[Message],
            _system: &str,
            _system_suffix: &str,
            _tools: &[ToolDefinition],
            _config: &CompletionConfig,
        ) -> Result<StreamResponse> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            self.gate.notified().await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(StreamResponse {
                stream: Box::pin(tokio_stream::iter(std::iter::empty())),
                tool_mode: ToolMode::Execute,
                input_tokens: None,
            })
        }

        fn set_mcp_tools(
            &self,
            _tools: std::collections::HashMap<String, std::sync::Arc<dyn Tool>>,
        ) {
        }
    }

    #[tokio::test]
    async fn concurrency_limit_caps_in_flight_calls() {
        let (inner, peak, gate) = OverlapTrackingClient::new();
        let client = Arc::new(ConcurrencyLimitedLlmClient::new(Box::new(inner), 2));

        // Spawn 6 callers; only 2 may be in flight at once.
        let mut handles = Vec::new();
        for _ in 0..6 {
            let c = Arc::clone(&client);
            handles.push(tokio::spawn(async move {
                c.stream(&[], "", "", &[], &empty_config()).await.unwrap();
            }));
        }

        // Let calls progress in waves of 2 by releasing the gate repeatedly.
        // notify_one wakes one waiter; loop until all 6 have completed.
        for _ in 0..6 {
            // Give spawned tasks a tick to enter the inner client.
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            gate.notify_one();
        }
        for h in handles {
            h.await.unwrap();
        }

        let observed_peak = peak.load(Ordering::SeqCst);
        assert!(
            observed_peak <= 2,
            "expected ≤2 concurrent calls, observed peak {observed_peak}"
        );
        assert!(observed_peak >= 1, "expected at least one call to land");
    }

    #[tokio::test]
    async fn concurrency_limit_zero_is_disabled_path_in_create_client() {
        // Direct test of the wrapper itself: with cap=1, 3 spawned callers
        // serialise (peak == 1).  Validates the semaphore is actually
        // applied — `create_client` skips wrapping when the cap is 0, so
        // that path stays unchanged.
        let (inner, peak, gate) = OverlapTrackingClient::new();
        let client = Arc::new(ConcurrencyLimitedLlmClient::new(Box::new(inner), 1));

        let mut handles = Vec::new();
        for _ in 0..3 {
            let c = Arc::clone(&client);
            handles.push(tokio::spawn(async move {
                c.stream(&[], "", "", &[], &empty_config()).await.unwrap();
            }));
        }
        for _ in 0..3 {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            gate.notify_one();
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }
}
