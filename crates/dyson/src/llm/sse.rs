//! Shared SSE plumbing for HTTP-streaming providers: buffer limits,
//! tool-call accumulation, line splitting, and provider error parsing.

use super::*;

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
            return Err(dyson_core::error::DysonError::Llm(
                "SSE line buffer exceeded 10 MB — aborting stream".into(),
            ));
        }
        self.buffer.extend_from_slice(bytes);

        let mut payloads = Vec::new();

        while let Some(newline_pos) = self.buffer.iter().position(|&b| b == b'\n') {
            // Decode the line from the buffer.  Lossy: an invalid UTF-8
            // byte becomes U+FFFD instead of dropping the whole payload
            // (the Cow borrows for the valid-UTF8 common case, so no
            // allocation happens unless the line is actually malformed).
            let line = String::from_utf8_lossy(&self.buffer[..newline_pos]);
            let line = line.trim();

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

/// Build a `DysonError` from a provider's HTTP error response.
///
/// All HTTP-based LLM providers go through this after their own
/// `parse_error_body` has stripped the JSON envelope, so the
/// wire-facing string is uniform across providers and statuses:
///
///   `{provider} ({status}): {message}`
///
/// The status-code → variant routing (429 → `LlmRateLimit`, 5xx →
/// `LlmOverloaded`, everything else → `Llm`) is identical across
/// providers and lives here once.
pub(crate) fn provider_http_error(
    provider: &str,
    status: reqwest::StatusCode,
    retry_after: Option<std::time::Duration>,
    message: &str,
) -> DysonError {
    let msg = format!("{provider} ({status}): {message}");
    match status.as_u16() {
        429 => DysonError::LlmRateLimit {
            message: msg,
            retry_after,
        },
        502 | 503 | 529 => DysonError::LlmOverloaded {
            message: msg,
            retry_after,
        },
        _ => DysonError::Llm(msg),
    }
}

/// Extract a human-readable message from a provider error body.
///
/// OpenAI, Anthropic, and Gemini all wrap errors as
/// `{ "error": { "message": "…" } }`; pull that message out, falling back
/// to the raw body when it's missing or the body isn't JSON.
pub(crate) fn parse_provider_error_body(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(err) = v.get("error")
    {
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(m) = message {
            return m.to_string();
        }
    }
    body.to_string()
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
        swarm_llm_audit_id: None,
        provider: None,
        model: None,
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
