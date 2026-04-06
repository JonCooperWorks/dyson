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
pub mod ollama_cloud;
pub mod openai;
pub mod openai_compat;
pub mod openrouter;
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
pub fn create_client(
    settings: &crate::config::AgentSettings,
    workspace: Option<std::sync::Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>>>,
    dangerous_no_sandbox: bool,
) -> Box<dyn LlmClient> {
    let entry = registry::lookup(&settings.provider);
    let config = registry::ClientConfig {
        api_key: settings.api_key.expose(),
        base_url: settings.base_url.as_deref(),
        workspace,
        dangerous_no_sandbox,
    };
    (entry.create_client)(&config)
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
    pub fn new() -> Self {
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
                .map(|s| s.trim())
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

/// Start an in-process MCP HTTP server exposing workspace tools.
///
/// Used by both `ClaudeCodeClient` and `CodexClient` to give CLI
/// subprocesses access to workspace tools via MCP.  Each `stream()` call
/// starts a fresh server on a random port; the returned `JoinHandle`
/// keeps it alive for the duration of the LLM turn.
pub(crate) async fn start_mcp_server(
    workspace: &std::sync::Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>>,
    dangerous_no_sandbox: bool,
) -> Result<McpServerInfo> {
    use crate::skill::mcp::serve::McpHttpServer;
    use std::sync::Arc;

    let server = Arc::new(McpHttpServer::new(
        Arc::clone(workspace),
        dangerous_no_sandbox,
    ));

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
}
