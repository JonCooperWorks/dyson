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
//   mod.rs         — LlmClient trait, CompletionConfig, ToolDefinition,
//                    shared utilities (format_prompt, resolve_binary_path)
//   stream.rs      — StreamEvent and StopReason enums
//   anthropic.rs   — Anthropic Messages API implementation
//   openai.rs      — OpenAI Chat Completions API (GPT, Ollama, etc.)
//   claude_code.rs — Claude Code CLI subprocess (no API key needed)
//   codex.rs       — Codex CLI subprocess (no API key needed)
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
use crate::message::{ContentBlock, Message, Role};

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
pub fn create_client(
    settings: &crate::config::AgentSettings,
    workspace: Option<std::sync::Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>>>,
    dangerous_no_sandbox: bool,
) -> Box<dyn LlmClient> {
    match settings.provider {
        crate::config::LlmProvider::Anthropic => Box::new(
            anthropic::AnthropicClient::new(
                settings.api_key.expose(),
                settings.base_url.as_deref(),
            ),
        ),
        crate::config::LlmProvider::OpenAi => Box::new(
            openai::OpenAiClient::new(
                settings.api_key.expose(),
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
/// Logs an error and falls back to an empty object if JSON parsing fails,
/// so the tool will still be called (and likely fail with a useful error
/// message about missing fields).
pub(crate) fn finalize_tool_call(buf: ToolCallBuffer) -> Result<StreamEvent> {
    let input = match serde_json::from_str(&buf.json) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(
                tool = buf.name,
                json = buf.json,
                error = %e,
                "failed to parse accumulated tool call JSON"
            );
            serde_json::json!({})
        }
    };

    Ok(StreamEvent::ToolUseComplete {
        id: buf.id,
        name: buf.name,
        input,
    })
}

// ---------------------------------------------------------------------------
// Shared utilities for CLI-subprocess-based clients
// ---------------------------------------------------------------------------

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
            tracing::warn!(binary = name, "could not resolve path — falling back to bare name");
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
pub(crate) fn format_prompt(messages: &[Message], tools: &[ToolDefinition]) -> String {
    // Single user message with no history and no tools — just return the text.
    if messages.len() == 1 && tools.is_empty() {
        if let Some(ContentBlock::Text { text }) = messages[0].content.first() {
            return text.clone();
        }
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
                        prompt.push_str(&format!("{role_label}: {text}\n\n"));
                    }
                    ContentBlock::ToolUse { name, input, .. } => {
                        prompt.push_str(&format!(
                            "[Used tool: {name} with input: {input}]\n\n"
                        ));
                    }
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        let label = if *is_error { "Tool error" } else { "Tool result" };
                        prompt.push_str(&format!("{label}: {content}\n\n"));
                    }
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
            prompt.push_str(&format!(
                "\n- **{}**: {}\n  Input schema: {}\n",
                tool.name, tool.description, tool.input_schema
            ));
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
        let tools = vec![ToolDefinition {
            name: "bash".into(),
            description: "Run commands".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let prompt = format_prompt(&messages, &tools);
        assert!(prompt.contains("[Available tools:]"));
        assert!(prompt.contains("**bash**"));
    }
}
