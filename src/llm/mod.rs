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
//   mod.rs       — LlmClient trait, CompletionConfig, ToolDefinition (this file)
//   stream.rs    — StreamEvent and StopReason enums
//   anthropic.rs — Anthropic Messages API implementation
//   openai.rs    — OpenAI Chat Completions API (also Codex, Ollama, etc.)
//   claude_code.rs — Claude Code CLI subprocess (no API key needed)
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
pub mod codex;
pub mod openai;
pub mod stream;

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;

use crate::error::Result;
use crate::llm::stream::StreamEvent;
use crate::message::Message;

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
}

// ---------------------------------------------------------------------------
// LlmClient trait
// ---------------------------------------------------------------------------

/// Provider-agnostic interface for streaming LLM completions.
///
/// Each provider (Anthropic, OpenAI, local) implements this trait.  The
/// agent loop calls `stream()` and consumes the resulting event stream
/// without knowing anything about the underlying API.
///
/// ## Why async?
///
/// LLM calls are network I/O — building the HTTP request, streaming the
/// response.  Async lets the tokio runtime do other work (handle Ctrl-C,
/// run the UI) while waiting for the first token.
///
/// ## Why Pin<Box<dyn Stream>>?
///
/// Streams in Rust are typically `!Unpin` (they contain internal state
/// that can't be moved).  `Pin<Box<...>>` is the standard way to return
/// a dynamically-dispatched, heap-allocated stream.  The `Send` bound
/// ensures the stream can be consumed from the tokio runtime.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Start a streaming completion.
    ///
    /// ## Parameters
    ///
    /// - `messages`: The conversation history (user messages, assistant
    ///   responses, tool results).
    /// - `system`: The system prompt (passed separately, not as a message).
    /// - `tools`: Available tool definitions (the LLM decides which to use).
    /// - `config`: Model, max_tokens, temperature.
    ///
    /// ## Returns
    ///
    /// A stream of `StreamEvent`s that the stream handler consumes.
    /// The stream ends with `StreamEvent::MessageComplete`.  Errors
    /// during streaming are emitted as `StreamEvent::Error` or as
    /// `Err(...)` items in the stream.
    async fn stream(
        &self,
        messages: &[Message],
        system: &str,
        tools: &[ToolDefinition],
        config: &CompletionConfig,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>>;

    /// Whether this provider runs its own internal tool-use loop.
    ///
    /// When `true`, the agent loop will NOT attempt to execute tool calls
    /// from the stream.  The provider (e.g., Claude Code) already executed
    /// them internally — the ToolUse stream events are informational only
    /// (displayed to the user but not re-executed by Dyson).
    ///
    /// Default is `false` (standard behavior: Dyson executes tool calls).
    fn handles_tools_internally(&self) -> bool {
        false
    }
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
/// - `workspace`: Shared workspace reference, used only by `ClaudeCodeClient`.
///   When `Some`, the Claude Code client starts an in-process HTTP MCP server
///   that exposes workspace tools (view, search, update) to the `claude` CLI.
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
/// | `ClaudeCode`   | Yes (MCP server)| Yes (forwarded)   | Claude Code built-in + workspace via MCP |
/// | `Codex`        | Yes (MCP server)| Yes (forwarded)   | Codex built-in + workspace via MCP |
///
/// ## Why workspace is passed here (not at stream time)
///
/// The workspace Arc is part of the client's configuration, not per-request
/// state.  All LLM turns within a session share the same workspace.  Passing
/// it at construction time simplifies the `LlmClient` trait (stream() doesn't
/// need workspace-awareness) and keeps the workspace coupling isolated to
/// the Claude Code backend.
pub fn create_client(
    settings: &crate::config::AgentSettings,
    workspace: Option<std::sync::Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>>>,
    dangerous_no_sandbox: bool,
) -> Box<dyn LlmClient> {
    match settings.provider {
        crate::config::LlmProvider::Anthropic => Box::new(
            anthropic::AnthropicClient::new(
                &settings.api_key,
                settings.base_url.as_deref(),
            ),
        ),
        crate::config::LlmProvider::OpenAi => Box::new(
            openai::OpenAiClient::new(
                &settings.api_key,
                settings.base_url.as_deref(),
            ),
        ),
        crate::config::LlmProvider::ClaudeCode => Box::new(
            claude_code::ClaudeCodeClient::new(
                settings.base_url.as_deref(),
                vec![], // MCP servers go through the skill system, not CLI args
                workspace,
                dangerous_no_sandbox,
            ),
        ),
        crate::config::LlmProvider::Codex => Box::new(
            codex::CodexClient::new(
                settings.base_url.as_deref(),
                workspace,
                dangerous_no_sandbox,
            ),
        ),
    }
}
