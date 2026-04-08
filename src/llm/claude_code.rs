// ===========================================================================
// Claude Code client — use the installed `claude` CLI as an LLM backend.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements `LlmClient` by spawning the locally installed `claude` CLI
//   as a subprocess.  This lets Dyson piggyback on Claude Code's
//   authentication, caching, and infrastructure without needing a separate
//   API key.  The user's existing Claude Code subscription "just works."
//
// Why use Claude Code as a backend?
//   1. **Zero config** — no API key needed.  If `claude` is installed and
//      authenticated, Dyson can use it immediately.
//   2. **Shared context cache** — Claude Code maintains prompt caching
//      across sessions.  Dyson benefits from cache hits.
//   3. **Model access** — Claude Code may have access to models that aren't
//      available through the raw API (beta models, organization-specific
//      deployments).
//   4. **Billing** — charges go through the user's existing Claude Code
//      subscription, not a separate API account.
//
// How it works:
//
//   Dyson spawns: claude -p \
//       --output-format stream-json \
//       --verbose \
//       --include-partial-messages \
//       --no-session-persistence \
//       --dangerously-skip-permissions \
//       --model <model> \
//       --append-system-prompt <system>
//
//   The key flags:
//     -p                          Print mode (non-interactive, pipe-friendly)
//     --output-format stream-json Emit newline-delimited JSON events
//     --verbose                   Required for stream-json output
//     --include-partial-messages  Emit raw Anthropic streaming events
//                                 (content_block_delta, etc.) for true
//                                 token-by-token streaming
//     --dangerously-skip-permissions  Always required: claude -p is
//                                     non-interactive and cannot answer
//                                     permission prompts
//     --no-session-persistence    Don't save to Claude Code's history
//     --append-system-prompt      Add our prompt ON TOP of Claude Code's
//                                 built-in prompt (preserves OS info, etc.)
//     --model                     Model selection
//
//   Dyson writes the user prompt to the subprocess's stdin, then reads
//   stdout line by line.  Each line is a JSON object.
//
// Stream-JSON event format:
//
//   With --include-partial-messages, Claude Code wraps the raw Anthropic
//   API streaming events in a JSON envelope:
//
//   {"type":"stream_event","event":{"type":"content_block_delta","index":0,
//     "delta":{"type":"text_delta","text":"Hello"}}}
//
//   This is EXACTLY the same event format our Anthropic SSE parser handles,
//   just delivered as JSON lines instead of SSE `data:` lines.  We reuse
//   the same SseParser by extracting the inner `event` object and feeding
//   it through `parse_sse_json()`.
//
// Other event types in the stream:
//
//   {"type":"system","subtype":"init","model":"...","tools":[...],...}
//     → Session initialization.  We extract the model name for logging.
//
//   {"type":"assistant","message":{"content":[{"type":"text","text":"..."}],...}}
//     → Complete assistant message (not partial).  We ignore these when
//       --include-partial-messages is active because we already got the
//       tokens via stream_event deltas.
//
//   {"type":"result","subtype":"success","result":"...","stop_reason":"end_turn",...}
//     → Final result.  We emit MessageComplete here.
//
//   {"type":"rate_limit_event",...}
//     → Rate limit info.  Logged but not surfaced to the agent.
//
//   {"type":"user","message":{"content":[{"type":"tool_result",...}]}}
//     → Claude Code's internal tool results.  We see these as Claude Code
//       executes tools in its own agent loop.
//
// Why let Claude Code keep its tools?
//   Claude Code has a full agent loop with Bash, Read, Write, Edit, etc.
//   — already sandboxed, already working.  Instead of duplicating that
//   in Dyson, we let Claude Code be the full agent and Dyson becomes the
//   transport layer (terminal, chat bots, etc.).  Claude Code handles
//   tool calls internally — Dyson streams the text output and tool
//   activity events to the user.
//
// Conversation history limitation:
//   The `claude -p` command is stateless — each invocation is a fresh
//   conversation.  To maintain multi-turn context, Dyson formats the
//   entire conversation history into a single prompt string.
//
// Tool calling:
//   Claude Code handles tool calling natively.  It has Bash, Read, Write,
//   Edit, Grep, etc. built in.  Dyson's own tool/sandbox system is NOT
//   used in this mode — Claude Code is the agent, Dyson is the transport.
//   The stream-json output includes tool_use and tool_result events so
//   Dyson can display what Claude Code is doing.
//
// Workspace tools via MCP:
//
//   Claude Code has its own tools, but it doesn't have Dyson's workspace
//   tools (workspace_view, workspace_search, workspace_update).  These
//   let the agent read/search/update its identity files (SOUL.md, etc.)
//   and memory/journal.
//
//   To bridge this gap, ClaudeCodeClient starts an in-process HTTP MCP
//   server (see skill/mcp/serve.rs) before spawning `claude -p`.  The
//   server exposes the workspace tools as MCP tools, and the config is
//   passed via `--mcp-config '<json>'` on the command line.
//
//   Sequence:
//     1. ClaudeCodeClient::stream() is called
//     2. If workspace is Some, start McpHttpServer on 127.0.0.1:random_port
//     3. Build MCP config JSON: {"mcpServers":{"dyson-workspace":{"type":"sse","url":"http://127.0.0.1:{port}/mcp"}}}
//     4. Pass as --mcp-config '<json>' CLI arg
//     5. Claude Code connects to the server, discovers tools
//     6. During its agent loop, Claude Code can call workspace_view etc.
//     7. When the stream ends, the server task is dropped (aborted)
//
//   This means Claude Code gets the full Dyson workspace experience
//   without Dyson needing to intercept or re-implement Claude Code's
//   tool execution loop.
//
// Sandbox plumbing:
//
//   The `dangerous_no_sandbox` flag is passed from the CLI through
//   Settings → create_client() → ClaudeCodeClient → McpHttpServer.
//   Today it has no effect (workspace tools are in-memory), but the
//   hook is in place for future sandbox enforcement of MCP tool calls.
// ===========================================================================

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;

use crate::error::{DysonError, Result};
use crate::llm::cli_subprocess::{self, CliLineParser, cli_event_stream};
use crate::llm::stream::{StopReason, StreamEvent};
use crate::llm::{CompletionConfig, LlmClient, ToolCallBuffer, ToolDefinition, finalize_tool_call};
use crate::message::Message;
use crate::tool::Tool;
use crate::workspace::Workspace;

// ---------------------------------------------------------------------------
// ClaudeCodeClient
// ---------------------------------------------------------------------------

/// LLM client that uses the locally installed `claude` CLI as its backend.
///
/// Spawns `claude -p --output-format stream-json` as a subprocess for each
/// LLM turn.  No API key required — uses Claude Code's stored credentials.
///
/// ## Limitations
///
/// - **No structured tool calling** — tool definitions are included in the
///   system prompt, but the model can't emit structured `tool_use` blocks
///   because the CLI doesn't expose the `tools` API parameter.  For
///   structured tool calling, use `AnthropicClient` or `OpenAiClient`.
///
/// - **Stateless** — each `stream()` call spawns a fresh `claude` process.
///   Conversation history is formatted into the prompt, not passed as
///   structured messages.
///
/// - **Requires `claude` in PATH** — the CLI must be installed and
///   authenticated (`claude auth login`).
pub struct ClaudeCodeClient {
    /// Path to the `claude` binary.
    ///
    /// Defaults to "claude" (found via PATH).  Can be overridden for
    /// custom installations (e.g., "/usr/local/bin/claude").
    claude_path: String,

    /// Optional MCP server configurations to pass via `--mcp-config`.
    ///
    /// Each entry is a JSON string that Claude Code parses as MCP server
    /// config.  Passed as `--mcp-config <json>` on the command line.
    mcp_configs: Vec<String>,

    /// Workspace to expose as MCP tools to Claude Code.
    ///
    /// When `Some`, each call to `stream()` will:
    /// 1. Start an `McpHttpServer` on 127.0.0.1:random_port (tokio task)
    /// 2. Build MCP config JSON pointing to `http://127.0.0.1:{port}/mcp`
    /// 3. Pass it to `claude -p` via `--mcp-config '<json>'`
    /// 4. Claude Code connects back to our server, discovers tools
    /// 5. The server lives until the stream is dropped (turn complete)
    ///
    /// This gives Claude Code access to workspace_view, workspace_search,
    /// and workspace_update as structured MCP tools with proper JSON schemas.
    ///
    /// When `None`, no MCP server is started and Claude Code runs without
    /// workspace tools.  This happens in tests or when no workspace is configured.
    workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,

    /// Whether sandbox enforcement is disabled (`--dangerous-no-sandbox`).
    ///
    /// Plumbed through to `McpHttpServer` for future sandbox gating of
    /// MCP tool calls.  Today workspace tools are pure in-memory operations
    /// (reading/writing a HashMap behind an RwLock) that don't need
    /// sandboxing, but the hook is here so that:
    ///
    /// 1. Adding sandbox enforcement later requires zero API changes
    /// 2. The flag flows consistently through the full chain:
    ///    CLI → Settings → create_client() → ClaudeCodeClient → McpHttpServer
    dangerous_no_sandbox: bool,

    /// Dyson tools exposed via MCP (set by agent via `set_mcp_tools`).
    mcp_tools: std::sync::Mutex<HashMap<String, Arc<dyn Tool>>>,
}

impl ClaudeCodeClient {
    /// Create a new Claude Code client.
    ///
    /// ## Parameters
    ///
    /// - `claude_path`: Path to the `claude` binary.  Pass `None` to auto-
    ///   resolve via `which claude`, falling back to bare `"claude"`.  The
    ///   path is resolved at construction time so service environments
    ///   (systemd, launchd) work even with a minimal PATH.
    ///
    /// - `mcp_configs`: Additional MCP server configs to pass via
    ///   `--mcp-config`.  Each entry is a raw JSON string.  Currently
    ///   always empty — external MCP servers go through Dyson's skill
    ///   system, not CLI args.  Kept for future direct pass-through.
    ///
    /// - `workspace`: If `Some`, the client will start an in-process HTTP
    ///   MCP server per `stream()` call, exposing workspace_view,
    ///   workspace_search, and workspace_update to Claude Code.  Pass
    ///   `None` to skip MCP server creation (no workspace tools).
    ///
    /// - `dangerous_no_sandbox`: Whether the `--dangerous-no-sandbox` CLI
    ///   flag was passed.  Forwarded to `McpHttpServer` for future sandbox
    ///   enforcement.  No effect today (workspace tools are in-memory).
    pub fn new(
        claude_path: Option<&str>,
        mcp_configs: Vec<String>,
        workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,
        dangerous_no_sandbox: bool,
    ) -> Self {
        let resolved = match claude_path {
            Some(p) => p.to_string(),
            None => super::resolve_binary_path("claude"),
        };

        Self {
            claude_path: resolved,
            mcp_configs,
            workspace,
            dangerous_no_sandbox,
            mcp_tools: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Build the CLI arguments for `claude -p`.
    ///
    /// Extracted as a method so the flag logic is unit-testable without
    /// spawning a subprocess.
    fn build_args(&self, model: &str, system: &str, mcp_config_json: Option<&str>) -> Vec<String> {
        let mut args = vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--include-partial-messages".to_string(),
            "--no-session-persistence".to_string(),
            // Always required: claude -p is non-interactive and cannot answer
            // permission prompts.  Without this flag Claude Code blocks or
            // refuses to run tools.
            "--dangerously-skip-permissions".to_string(),
            "--model".to_string(),
            model.to_string(),
            "--append-system-prompt".to_string(),
            system.to_string(),
        ];

        for mcp_json in &self.mcp_configs {
            args.push("--mcp-config".to_string());
            args.push(mcp_json.clone());
        }

        if let Some(json) = mcp_config_json {
            args.push("--mcp-config".to_string());
            args.push(json.to_string());
        }

        args
    }
}

// ---------------------------------------------------------------------------
// LlmClient implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmClient for ClaudeCodeClient {
    /// Stream a completion by spawning the `claude` CLI.
    ///
    /// ## Process lifecycle
    ///
    /// 1. Build the command with appropriate flags
    /// 2. Spawn the subprocess
    /// 3. Write the formatted prompt to stdin, then close it
    /// 4. Read stdout line by line, parsing JSON events
    /// 5. Map events to `StreamEvent`s
    /// 6. The stream ends when the process exits
    ///
    /// ## Flag selection
    ///
    /// | Flag | Purpose |
    /// |------|---------|
    /// | `-p` | Print mode — non-interactive, reads stdin, writes stdout |
    /// | `--output-format stream-json` | Newline-delimited JSON events |
    /// | `--verbose` | Required for stream-json (CLI enforces this) |
    /// | `--include-partial-messages` | Raw Anthropic streaming events |
    /// | `--dangerously-skip-permissions` | Non-interactive — can't answer prompts |
    /// | `--no-session-persistence` | Don't save this to Claude Code's history |
    /// | `--model <model>` | Model selection |
    /// | `--append-system-prompt <prompt>` | System prompt (on top of built-in) |
    async fn stream(
        &self,
        messages: &[Message],
        system: &str,
        system_suffix: &str,
        tools: &[ToolDefinition],
        config: &CompletionConfig,
    ) -> Result<crate::llm::StreamResponse> {
        // -- Format conversation history into a prompt string --
        //
        // The claude CLI in -p mode takes a single prompt.  We format the
        // entire conversation history into a readable string so the model
        // has context from previous turns.
        //
        // When MCP is active, tools are structured — skip text descriptions.
        let prompt_tools = cli_subprocess::filter_tools_for_cli(tools, self.workspace.is_some());
        let prompt = super::format_prompt(messages, &prompt_tools);

        tracing::debug!(
            model = config.model,
            message_count = messages.len(),
            tool_count = tools.len(),
            prompt_len = prompt.len(),
            "spawning claude CLI"
        );

        // -- Start MCP server if workspace is available --
        //
        // The server lives as a tokio task for the duration of the LLM turn.
        // When the stream is dropped, the handle is dropped, stopping the server.
        let mut _mcp_server_handle: Option<tokio::task::JoinHandle<()>> = None;
        let mut mcp_config_json: Option<String> = None;

        if let Some(ref workspace) = self.workspace {
            let extra = self.mcp_tools.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let info = super::start_mcp_server(workspace, self.dangerous_no_sandbox, &extra).await?;

            // Build MCP config JSON for Claude Code's --mcp-config flag.
            let config = serde_json::json!({
                "mcpServers": {
                    "dyson-workspace": {
                        "type": "sse",
                        "url": info.url,
                        "headers": {
                            "Authorization": format!("Bearer {}", info.token)
                        }
                    }
                }
            });

            tracing::info!(port = info.port, "MCP server started for Claude Code");

            mcp_config_json = Some(config.to_string());
            _mcp_server_handle = Some(info.handle);
        }

        // -- Build the command --
        // Concatenate stable system prompt and ephemeral suffix (Claude Code
        // CLI takes a single --append-system-prompt string, no caching split).
        let full_system = if system_suffix.is_empty() {
            system.to_string()
        } else {
            format!("{system}\n\n{system_suffix}")
        };
        let args = self.build_args(&config.model, &full_system, mcp_config_json.as_deref());

        let mut cmd = tokio::process::Command::new(&self.claude_path);
        for arg in &args {
            cmd.arg(arg);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        // -- Spawn the process --
        let mut child = cmd.spawn().map_err(|e| {
            DysonError::Llm(format!(
                "failed to spawn '{}': {e}.  Is Claude Code installed?  \
                 Install with: npm install -g @anthropic/claude-code",
                self.claude_path
            ))
        })?;

        // -- Write prompt to stdin and close it --
        //
        // We must close stdin so claude knows the input is complete.
        // The `take()` gives us ownership of the stdin handle; dropping
        // it closes the pipe.
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| DysonError::Llm("failed to open stdin for claude process".into()))?;

        // Write in a spawned task so we don't block the stream setup.
        // For large conversation histories, this could take a moment.
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
            let _ = stdin.flush().await;
            // stdin is dropped here, closing the pipe.
        });

        // -- Read stdout line by line and parse JSON events --
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DysonError::Llm("failed to open stdout for claude process".into()))?;

        // Keep child process and MCP server alive for the stream's lifetime.
        let mut keep_alive: Vec<Box<dyn std::any::Any + Send>> = vec![Box::new(child)];
        if let Some(handle) = _mcp_server_handle {
            keep_alive.push(Box::new(handle));
        }

        let event_stream = cli_event_stream(stdout, StreamParserState::new(), keep_alive);

        Ok(cli_subprocess::build_observe_response(event_stream))
    }

    fn set_mcp_tools(&self, tools: HashMap<String, Arc<dyn Tool>>) {
        let filtered: HashMap<_, _> = tools.into_iter().filter(|(_, t)| !t.agent_only()).collect();
        tracing::info!(tool_count = filtered.len(), "MCP tools registered");
        *self.mcp_tools.lock().unwrap_or_else(|e| e.into_inner()) = filtered;
    }
}

// ---------------------------------------------------------------------------
// StreamParserState — the single source of truth for event parsing.
// ---------------------------------------------------------------------------

/// Mutable state for parsing Claude Code's stream-json output line by line.
///
/// This is the single source of truth for Claude Code event parsing.  Used by
/// both the `stream()` async closure (production) and unit tests.
struct StreamParserState {
    completed: bool,
    got_stream_deltas: bool,
    tool_buffers: HashMap<usize, ToolCallBuffer>,
    thinking_blocks: std::collections::HashSet<usize>,
}

impl CliLineParser for StreamParserState {
    fn parse_line(&mut self, line: &str) -> Vec<Result<StreamEvent>> {
        let mut events = Vec::new();

        let json: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return events,
        };

        let event_type = json["type"].as_str().unwrap_or("");

        match event_type {
            "stream_event" => {
                let inner = &json["event"];
                let inner_type = inner["type"].as_str().unwrap_or("");

                match inner_type {
                    "content_block_delta" => {
                        let delta = &inner["delta"];
                        match delta["type"].as_str().unwrap_or("") {
                            "thinking_delta" => {
                                if let Some(text) = delta["thinking"].as_str() {
                                    events.push(Ok(StreamEvent::ThinkingDelta(text.to_string())));
                                }
                            }
                            "text_delta" => {
                                if let Some(text) = delta["text"].as_str() {
                                    let idx = inner["index"].as_u64().unwrap_or(0) as usize;
                                    if self.thinking_blocks.contains(&idx) {
                                        events
                                            .push(Ok(StreamEvent::ThinkingDelta(text.to_string())));
                                    } else {
                                        self.got_stream_deltas = true;
                                        events.push(Ok(StreamEvent::TextDelta(text.to_string())));
                                    }
                                }
                            }
                            "input_json_delta" => {
                                if let Some(partial) = delta["partial_json"].as_str() {
                                    let idx = inner["index"].as_u64().unwrap_or(0) as usize;
                                    if let Some(buf) = self.tool_buffers.get_mut(&idx) {
                                        buf.json.push_str(partial);
                                    }
                                    events.push(Ok(StreamEvent::ToolUseInputDelta(
                                        partial.to_string(),
                                    )));
                                }
                            }
                            _ => {}
                        }
                    }

                    "content_block_start" => {
                        let block = &inner["content_block"];
                        let block_type = block["type"].as_str().unwrap_or("");
                        let idx = inner["index"].as_u64().unwrap_or(0) as usize;

                        if block_type == "tool_use" {
                            let id = block["id"].as_str().unwrap_or("").to_string();
                            let name = block["name"].as_str().unwrap_or("").to_string();
                            self.tool_buffers.insert(
                                idx,
                                ToolCallBuffer {
                                    id: id.clone(),
                                    name: name.clone(),
                                    json: String::new(),
                                },
                            );
                            events.push(Ok(StreamEvent::ToolUseStart { id, name }));
                        } else if block_type == "thinking" {
                            self.thinking_blocks.insert(idx);
                        }
                    }

                    "content_block_stop" => {
                        let idx = inner["index"].as_u64().unwrap_or(0) as usize;
                        if let Some(buf) = self.tool_buffers.remove(&idx) {
                            events.push(finalize_tool_call(buf));
                        }
                    }

                    _ => {}
                }
            }

            "result" => {
                if !self.completed {
                    self.completed = true;
                    if json["is_error"].as_bool() == Some(true) {
                        let error_msg = json["result"].as_str().unwrap_or("unknown error");
                        events.push(Err(DysonError::Llm(format!(
                            "Claude Code error: {error_msg}"
                        ))));
                    } else {
                        let stop_reason = match json["stop_reason"].as_str() {
                            Some("end_turn") => StopReason::EndTurn,
                            Some("tool_use") => StopReason::ToolUse,
                            Some("max_tokens") => StopReason::MaxTokens,
                            _ => StopReason::EndTurn,
                        };
                        events.push(Ok(StreamEvent::MessageComplete {
                            stop_reason,
                            output_tokens: None,
                        }));
                    }
                }
            }

            "assistant" => {
                if !self.got_stream_deltas
                    && let Some(content) = json["message"]["content"].as_array()
                {
                    for block in content {
                        if block["type"].as_str() == Some("text")
                            && let Some(text) = block["text"].as_str()
                            && !text.is_empty()
                        {
                            events.push(Ok(StreamEvent::TextDelta(text.to_string())));
                        }
                    }
                }
                self.got_stream_deltas = false;
                self.thinking_blocks.clear();
                self.tool_buffers.clear();
            }

            _ => {}
        }

        events
    }

    fn finalize(&mut self) -> Vec<Result<StreamEvent>> {
        let mut events = Vec::new();
        if !self.completed {
            events.push(Err(DysonError::Llm(
                "Claude Code process exited without producing a result".to_string(),
            )));
        }
        events
    }
}

impl StreamParserState {
    fn new() -> Self {
        Self {
            completed: false,
            got_stream_deltas: false,
            tool_buffers: HashMap::new(),
            thinking_blocks: std::collections::HashSet::new(),
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // JSON event parsing tests
    //
    // These test the event types we parse from Claude Code's stream-json
    // output.  We can't easily test the full stream() method (it spawns
    // a subprocess), but we can verify the JSON parsing logic.
    // -----------------------------------------------------------------------

    #[test]
    fn parse_stream_event_text_delta() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}}"#
        ).unwrap();

        let event_type = json["type"].as_str().unwrap();
        assert_eq!(event_type, "stream_event");

        let inner = &json["event"];
        let delta = &inner["delta"];
        assert_eq!(delta["type"].as_str().unwrap(), "text_delta");
        assert_eq!(delta["text"].as_str().unwrap(), "Hello");
    }

    #[test]
    fn parse_result_event() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","total_cost_usd":0.01,"duration_ms":1234}"#
        ).unwrap();

        assert_eq!(json["type"].as_str().unwrap(), "result");
        assert_eq!(json["stop_reason"].as_str().unwrap(), "end_turn");
        assert_eq!(json["total_cost_usd"].as_f64().unwrap(), 0.01);
    }

    #[test]
    fn parse_system_init_event() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"type":"system","subtype":"init","model":"claude-sonnet-4-6","tools":[],"claude_code_version":"2.1.76"}"#
        ).unwrap();

        assert_eq!(json["type"].as_str().unwrap(), "system");
        assert_eq!(json["model"].as_str().unwrap(), "claude-sonnet-4-6");
    }

    // -----------------------------------------------------------------------
    // Bug reproduction tests (StreamParserState)
    // -----------------------------------------------------------------------

    #[test]
    fn error_result_yields_error() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"result","subtype":"error","is_error":true,"result":"Rate limit exceeded","duration_ms":100,"total_cost_usd":0.0}"#
        );
        assert_eq!(events.len(), 1);
        assert!(
            events[0].is_err(),
            "error result should yield Err, not Ok(MessageComplete)"
        );
        let err_msg = format!("{}", events[0].as_ref().unwrap_err());
        assert!(err_msg.contains("Rate limit exceeded"));
    }

    #[test]
    fn success_result_yields_message_complete() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","total_cost_usd":0.01,"duration_ms":1234}"#
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(StreamEvent::MessageComplete { stop_reason, .. }) => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
            }
            other => panic!("expected Ok(MessageComplete), got: {other:?}"),
        }
    }

    #[test]
    fn no_result_event_yields_error_on_finalize() {
        let mut state = StreamParserState::new();
        state.parse_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}}"#
        );
        let final_events = state.finalize();
        assert_eq!(final_events.len(), 1);
        assert!(
            final_events[0].is_err(),
            "finalize without result should yield Err"
        );
    }

    #[test]
    fn finalize_after_result_produces_nothing() {
        let mut state = StreamParserState::new();
        state.parse_line(
            r#"{"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","total_cost_usd":0.0,"duration_ms":0}"#
        );
        let final_events = state.finalize();
        assert!(final_events.is_empty());
    }

    #[test]
    fn thinking_blocks_cleared_between_turns() {
        let mut state = StreamParserState::new();

        // Turn 1: index 0 is a thinking block.
        state.parse_line(
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}}"#
        );
        let events = state.parse_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"reasoning..."}}}"#
        );
        match &events[0] {
            Ok(StreamEvent::ThinkingDelta(t)) => assert_eq!(t, "reasoning..."),
            other => panic!("turn 1: expected ThinkingDelta, got: {other:?}"),
        }

        // Turn boundary.
        state.parse_line(r#"{"type":"assistant","message":{"content":[]}}"#);

        // Turn 2: index 0 is now a regular text block.
        state.parse_line(
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text"}}}"#
        );
        let events = state.parse_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"visible answer"}}}"#
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(StreamEvent::TextDelta(t)) => assert_eq!(t, "visible answer"),
            other => panic!("turn 2: expected TextDelta, got: {other:?}"),
        }
    }

    #[test]
    fn tool_buffers_cleared_between_turns() {
        let mut state = StreamParserState::new();

        state.parse_line(
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_1","name":"bash"}}}"#
        );
        assert!(state.tool_buffers.contains_key(&1));

        state.parse_line(r#"{"type":"assistant","message":{"content":[]}}"#);

        assert!(
            state.tool_buffers.is_empty(),
            "tool_buffers should be cleared on turn boundary"
        );
    }

    // -----------------------------------------------------------------------
    // build_args tests
    // -----------------------------------------------------------------------

    #[test]
    fn build_args_always_includes_skip_permissions() {
        // claude -p is non-interactive — it MUST always pass
        // --dangerously-skip-permissions regardless of the sandbox flag.
        let client = ClaudeCodeClient::new(
            Some("/usr/bin/claude"),
            vec![],
            None,
            false, // sandbox NOT disabled
        );
        let args = client.build_args("sonnet", "be helpful", None);
        assert!(
            args.contains(&"--dangerously-skip-permissions".to_string()),
            "must always include --dangerously-skip-permissions for non-interactive mode"
        );
    }

    #[test]
    fn build_args_includes_model_and_system_prompt() {
        let client = ClaudeCodeClient::new(Some("claude"), vec![], None, false);
        let args = client.build_args("claude-opus-4-20250514", "You are Dyson", None);
        assert!(args.contains(&"claude-opus-4-20250514".to_string()));
        assert!(args.contains(&"You are Dyson".to_string()));
        assert!(args.contains(&"--append-system-prompt".to_string()));
    }

    #[test]
    fn build_args_includes_mcp_config() {
        let client = ClaudeCodeClient::new(Some("claude"), vec![], None, false);
        let mcp_json = r#"{"mcpServers":{"test":{"type":"sse","url":"http://localhost:1234"}}}"#;
        let args = client.build_args("sonnet", "", Some(mcp_json));
        assert!(args.contains(&"--mcp-config".to_string()));
        assert!(args.contains(&mcp_json.to_string()));
    }

    #[test]
    fn build_args_forwards_extra_mcp_configs() {
        let extra = r#"{"mcpServers":{"extra":{"type":"stdio"}}}"#.to_string();
        let client = ClaudeCodeClient::new(Some("claude"), vec![extra.clone()], None, false);
        let args = client.build_args("sonnet", "", None);
        assert!(args.contains(&extra));
    }
}
