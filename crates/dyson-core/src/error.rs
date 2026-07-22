// ===========================================================================
// Error types — every fallible operation in Dyson flows through DysonError.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Defines a single, unified error enum (`DysonError`) that every module in
//   Dyson uses for its `Result` types.  Having one error type across the
//   entire crate means callers never need to juggle multiple error types or
//   write ad-hoc conversions — `?` just works everywhere.
//
// Why one enum instead of per-module errors?
//   Dyson is a pipeline: config → skill/tool → LLM → agent loop → UI.
//   Errors bubble across module boundaries constantly (an MCP tool error
//   surfaces through the agent loop into the UI).  A single enum avoids
//   the nested-error-type problem where you end up with
//   `AgentError(ToolError(McpError(IoError)))`.  The trade-off is a
//   slightly larger enum, but the ergonomic win is massive.
//
// How thiserror works here:
//   The `#[derive(thiserror::Error)]` macro auto-generates `Display` and
//   `Error` impls.  `#[from]` attributes generate `From<T>` impls so
//   `std::io::Error`, `reqwest::Error`, and `serde_json::Error` can be
//   converted with `?` automatically.  Variants like `Tool` and `Mcp` use
//   named fields instead of `#[from]` because they carry extra context
//   (which tool failed, which server).
// ===========================================================================

// ---------------------------------------------------------------------------
// DysonError
// ---------------------------------------------------------------------------

/// Unified error type for the entire Dyson crate.
///
/// Every `Result<T>` in this crate is shorthand for `Result<T, DysonError>`.
/// Variants are grouped by subsystem so match arms read naturally:
///
/// ```ignore
/// match err {
///     DysonError::Llm(msg) => /* LLM provider issue */,
///     DysonError::Tool { tool, message } => /* a tool failed */,
///     DysonError::Config(msg) => /* bad config */,
///     _ => /* infrastructure: IO, HTTP, JSON */,
/// }
/// ```
#[derive(Debug, thiserror::Error)]
pub enum DysonError {
    /// An error from the LLM provider (API rejection, etc.).
    #[error("LLM error: {0}")]
    Llm(String),

    /// The LLM provider returned a rate limit error (HTTP 429 or equivalent).
    ///
    /// `retry_after` carries the server's wait hint when one was present
    /// (`Retry-After` header or `X-RateLimit-Reset`).  `RetryingLlmClient`
    /// uses it as a floor under the exponential backoff so we don't retry
    /// inside a still-open rate-limit window and burn another quota slot.
    #[error("LLM rate limited: {message}")]
    LlmRateLimit {
        message: String,
        retry_after: Option<std::time::Duration>,
    },

    /// The LLM provider is overloaded (HTTP 529, 502, 503, or equivalent).
    /// `retry_after` carries the server's wait hint when present.
    #[error("LLM overloaded: {message}")]
    LlmOverloaded {
        message: String,
        retry_after: Option<std::time::Duration>,
    },

    /// A tool failed during execution.
    ///
    /// `tool` is the tool's registered name (e.g. "bash"), `message` is
    /// the human-readable explanation.  The agent loop catches these and
    /// sends the message back to the LLM as an error tool_result so it
    /// can retry or adjust.
    #[error("Tool error [{tool}]: {message}")]
    Tool { tool: String, message: String },

    /// An MCP (Model Context Protocol) server returned an error or
    /// the transport failed.
    #[error("MCP error [{server}]: {message}")]
    Mcp { server: String, message: String },

    /// An OAuth 2.0 flow failed (discovery, token exchange, refresh, etc.).
    #[error("OAuth error [{server}]: {message}")]
    OAuth { server: String, message: String },

    /// Configuration is missing, malformed, or contradictory.
    #[error("Config error: {0}")]
    Config(String),

    /// Filesystem or other I/O failure.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// HTTP transport failure (reqwest).
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON serialization or deserialization failure.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// The operation was cancelled (e.g. Ctrl-C during a tool call).
    #[error("Cancelled")]
    Cancelled,

    /// Rate limit exceeded — too many messages in the time window.
    #[error("Rate limited: {limit} messages per {window_secs}s exceeded")]
    RateLimit { limit: usize, window_secs: u64 },
}

// ---------------------------------------------------------------------------
// LLM error recovery
// ---------------------------------------------------------------------------

/// Recovery action a controller requests after an LLM error.
///
/// Returned by [`Output::on_llm_error`] to tell the agent loop how to
/// proceed when a non-retryable LLM error occurs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmRecovery {
    /// Abandon this turn — propagate the error to the caller.
    GiveUp,
    /// Disable tools, strip tool history, and retry the user message.
    RetryWithoutTools,
    /// Strip images from history and retry the user message.
    RetryWithoutImages,
}

/// Coarse classification of an LLM error for recovery decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmErrorKind {
    /// The model doesn't support tool/function calling.
    NoToolUse,
    /// The model doesn't support image/vision input.
    NoVision,
    /// Any other (unrecoverable) error.
    Other,
}

/// Classify an LLM error string so controllers can decide on recovery.
pub fn classify_llm_error(err: &str) -> LlmErrorKind {
    if err.contains("tool use") || err.contains("tool_use") {
        LlmErrorKind::NoToolUse
    } else if err.contains("image input")
        || err.contains("vision")
        || err.contains("No endpoints found that support image")
        || err.contains("image(s) may be provided")
        || err.contains("unsupported content type 'image_url'")
    {
        LlmErrorKind::NoVision
    } else {
        LlmErrorKind::Other
    }
}

// ---------------------------------------------------------------------------
// Convenience type alias
// ---------------------------------------------------------------------------

/// Crate-wide result type.  Every function that can fail returns this.
pub type Result<T> = std::result::Result<T, DysonError>;

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

impl DysonError {
    /// Convenience constructor for tool errors.
    pub fn tool(tool: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Tool {
            tool: tool.into(),
            message: message.into(),
        }
    }

    /// Convenience constructor for MCP errors.
    pub fn mcp(server: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Mcp {
            server: server.into(),
            message: message.into(),
        }
    }

    /// Convenience constructor for OAuth errors.
    pub fn oauth(server: impl Into<String>, message: impl Into<String>) -> Self {
        Self::OAuth {
            server: server.into(),
            message: message.into(),
        }
    }

    /// Returns a redacted error message safe to send to a remote client.
    ///
    /// `Display` (used by `to_string()`) leaks infrastructure detail that
    /// callers don't need and attackers can mine: `Io` exposes filesystem
    /// paths, `Http` exposes upstream URLs and status text from the
    /// provider, `Json` exposes input bytes around the parse error.  In a
    /// single-user deployment that's self-info, but a multi-tenant OIDC
    /// deployment streams these errors over SSE to whoever made the
    /// request — including across tenants if the chat is shared.
    ///
    /// LLM/Tool/MCP/OAuth/Rate-limit errors carry application-level
    /// messages the model and the user actually need to see, so they
    /// pass through unchanged.  Infrastructure variants collapse to a
    /// generic category.  Full `Display` output remains available for
    /// server-side logs.
    pub fn sanitized_message(&self) -> String {
        match self {
            Self::Llm(_)
            | Self::LlmRateLimit { .. }
            | Self::LlmOverloaded { .. }
            | Self::Tool { .. }
            | Self::Mcp { .. }
            | Self::OAuth { .. }
            | Self::Config(_)
            | Self::RateLimit { .. }
            | Self::Cancelled => self.to_string(),
            Self::Io(_) => "internal IO error".to_string(),
            Self::Http(_) => "upstream HTTP error".to_string(),
            Self::Json(_) => "internal JSON error".to_string(),
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_messages() {
        let e = DysonError::Llm("rate limited".into());
        assert_eq!(e.to_string(), "LLM error: rate limited");

        let e = DysonError::tool("bash", "command not found");
        assert_eq!(e.to_string(), "Tool error [bash]: command not found");

        let e = DysonError::mcp("github", "connection refused");
        assert_eq!(e.to_string(), "MCP error [github]: connection refused");

        let e = DysonError::Config("missing API key".into());
        assert_eq!(e.to_string(), "Config error: missing API key");

        let e = DysonError::Cancelled;
        assert_eq!(e.to_string(), "Cancelled");
    }

    #[test]
    fn io_error_converts() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let dyson_err: DysonError = io_err.into();
        assert!(dyson_err.to_string().contains("gone"));
    }

    #[test]
    fn classify_llm_error_detects_no_tool_use() {
        assert_eq!(
            classify_llm_error("does not support tool use"),
            LlmErrorKind::NoToolUse
        );
        assert_eq!(
            classify_llm_error("tool_use not available"),
            LlmErrorKind::NoToolUse
        );
    }

    #[test]
    fn classify_llm_error_detects_no_vision() {
        assert_eq!(
            classify_llm_error("does not support image input"),
            LlmErrorKind::NoVision
        );
        assert_eq!(
            classify_llm_error("vision not supported"),
            LlmErrorKind::NoVision
        );
        assert_eq!(
            classify_llm_error("No endpoints found that support image input"),
            LlmErrorKind::NoVision
        );
        assert_eq!(
            classify_llm_error("At most 0 image(s) may be provided in one prompt."),
            LlmErrorKind::NoVision
        );
        assert_eq!(
            classify_llm_error("unsupported content type 'image_url'"),
            LlmErrorKind::NoVision
        );
    }

    #[test]
    fn classify_llm_error_generic_no_endpoints_is_other() {
        // "No endpoints found" without image/vision context should NOT be
        // classified as NoVision — it usually means the model is unavailable.
        assert_eq!(
            classify_llm_error("No endpoints found"),
            LlmErrorKind::Other
        );
        assert_eq!(
            classify_llm_error("No endpoints found for this model"),
            LlmErrorKind::Other,
        );
    }

    #[test]
    fn sanitized_message_redacts_infrastructure_errors() {
        // IO/HTTP/JSON variants leak filesystem paths, upstream URLs, and
        // input bytes through their Display impls.  Sanitised form must
        // collapse to a generic category — full detail still goes to logs
        // via to_string().
        let io_err = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "/home/user/.secret/private.key: permission denied",
        );
        let dyson_err: DysonError = io_err.into();
        assert!(dyson_err.to_string().contains("/home/user/.secret"));
        assert_eq!(dyson_err.sanitized_message(), "internal IO error");
        assert!(!dyson_err.sanitized_message().contains("/"));
    }

    #[test]
    fn sanitized_message_passes_through_application_errors() {
        // LLM/Tool/MCP/OAuth/Config/RateLimit messages are application
        // text the user and model actually need to see — pass through.
        let cases: Vec<DysonError> = vec![
            DysonError::Llm("rate limited".into()),
            DysonError::tool("bash", "command not found"),
            DysonError::mcp("github", "connection refused"),
            DysonError::oauth("google", "token expired"),
            DysonError::Config("missing API key".into()),
            DysonError::RateLimit {
                limit: 10,
                window_secs: 60,
            },
            DysonError::Cancelled,
        ];
        for e in cases {
            assert_eq!(
                e.sanitized_message(),
                e.to_string(),
                "application error must pass through: {e:?}"
            );
        }
    }

    #[test]
    fn classify_llm_error_defaults_to_other() {
        assert_eq!(classify_llm_error("rate limited"), LlmErrorKind::Other);
        assert_eq!(
            classify_llm_error("internal server error"),
            LlmErrorKind::Other
        );
    }
}
