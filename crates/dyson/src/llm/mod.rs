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
//                     create_client, and re-exports of the modules below
//   sse.rs          — shared SSE limits, tool-call buffers, error parsing
//   decorators.rs   — retry and concurrency-cap LlmClient wrappers
//   cli_support.rs  — CLI-subprocess helpers (MCP server, format_prompt)
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
pub mod openai;
pub mod openai_compat;
pub(crate) mod openai_responses;
pub mod pricing;
pub mod registry;
pub(crate) mod sse_parser;
pub mod stream;
pub(crate) mod wire_tools;

mod cli_support;
mod decorators;
mod sse;

// Facade: the split-out modules keep their historical `crate::llm::*` paths.
pub(crate) use cli_support::{
    format_prompt, resolve_binary_path, start_mcp_server, start_mcp_tools_server,
};
pub use decorators::is_retryable;
pub(crate) use decorators::{ConcurrencyLimitedLlmClient, RetryingLlmClient};
pub(crate) use sse::{
    MAX_ACTIVE_TOOL_BUFFERS, MAX_TOOL_JSON, MAX_TOTAL_TOOL_JSON, SseLineBuffer, SseStreamParser,
    ToolCallBuffer, build_stream_response, concat_system_prompt, finalize_tool_call,
    parse_provider_error_body, parse_retry_hint, provider_http_error,
};

use std::pin::Pin;

use async_trait::async_trait;
use tokio_stream::Stream;

use std::fmt::Write as _;

use crate::llm::stream::StreamEvent;
use crate::tool::Tool;
use dyson_core::error::{DysonError, Result};
use dyson_core::message::{ContentBlock, Message, Role};

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

    /// Swarm LLM audit row id, passed as a side-channel response header by
    /// the Swarm proxy. It is display/accounting metadata only.
    pub swarm_llm_audit_id: Option<i64>,

    /// Provider/model metadata associated with the stream for message display.
    pub provider: Option<String>,
    pub model: Option<String>,
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
// `#[async_trait]` emits a bare `#[must_use]` on boxed-future trait methods.
#[allow(clippy::double_must_use)]
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
    /// - `tool_instances`: Concrete implementations for this call. CLI
    ///   providers expose the visible subset through their per-turn MCP server;
    ///   API providers ignore them and use only the definitions.
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
        tool_instances: &std::collections::HashMap<String, std::sync::Arc<dyn Tool>>,
        config: &CompletionConfig,
    ) -> Result<StreamResponse>;
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
    sandbox_bypass: Option<&crate::sandbox::SandboxBypassGuard>,
) -> Box<dyn LlmClient> {
    let entry = registry::lookup(&settings.provider);
    let config = registry::ClientConfig {
        api_key: settings.api_key.expose(),
        base_url: settings.base_url.as_deref(),
        workspace,
        sandbox_bypass,
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

#[cfg(test)]
mod tests;
