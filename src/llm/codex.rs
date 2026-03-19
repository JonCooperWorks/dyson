// ===========================================================================
// Codex CLI client — use the installed `codex` CLI as an LLM backend.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements `LlmClient` by spawning the locally installed `codex` CLI
//   (OpenAI's Codex CLI) as a subprocess.  This lets Dyson use OpenAI models
//   through Codex's agent loop, similar to how `claude_code.rs` uses the
//   Claude Code CLI.
//
// Why use Codex as a backend?
//   1. **Zero config** — no API key needed if `codex` is already authenticated.
//   2. **Built-in agent loop** — Codex has its own shell execution, file
//      editing, MCP support, and web search.
//   3. **OpenAI models** — access to o3, o4-mini, and other OpenAI models
//      through the user's existing Codex subscription.
//   4. **Sandboxing** — Codex has its own sandbox system for shell commands.
//
// How it works:
//
//   Dyson spawns: codex exec \
//       --json \
//       --dangerously-bypass-approvals-and-sandbox \
//       --ephemeral \
//       --model <model> \
//       -c developer_instructions="<system>" \
//       "<prompt>"
//
//   The key flags:
//     exec                                Non-interactive mode
//     --json                              Emit JSONL events to stdout
//     --dangerously-bypass-approvals-and-sandbox
//                                         Skip permission prompts (non-interactive)
//     --ephemeral                         Don't persist session files
//     --model                             Model selection
//     -c developer_instructions="..."     Inject system prompt
//
//   Codex writes JSONL events to stdout.  Each line is a JSON object with
//   a "type" field that determines the event kind.
//
// JSONL event types:
//
//   thread.started  — Session initialized with thread_id
//   turn.started    — A new LLM turn begins
//   turn.completed  — Turn finished successfully (includes usage)
//   turn.failed     — Turn ended with an error
//   item.started    — Tool execution began (command, MCP call, etc.)
//   item.completed  — Tool execution finished or agent message received
//   error           — Stream-level error
//
// Item types within item events:
//
//   agent_message       — The model's text response
//   reasoning           — Internal reasoning summary
//   command_execution   — Shell command with output
//   file_change         — File modifications
//   mcp_tool_call       — MCP server tool invocation
//   web_search          — Web search
//
// Why let Codex keep its tools?
//   Same rationale as Claude Code — Codex has a full agent loop with
//   shell execution, file ops, MCP, etc.  Dyson acts as the transport
//   layer, streaming events to the user.
//
// Conversation history:
//   `codex exec` is stateless.  Multi-turn context is formatted into
//   a single prompt string, identical to the Claude Code approach.
//   We reuse the same `format_prompt()` logic.
// ===========================================================================

use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::RwLock;

use crate::error::{DysonError, Result};
use crate::llm::stream::{StopReason, StreamEvent};
use crate::llm::{CompletionConfig, LlmClient, ToolDefinition};
use crate::message::Message;
use crate::skill::mcp::serve::McpHttpServer;
use crate::workspace::Workspace;

// ---------------------------------------------------------------------------
// CodexClient
// ---------------------------------------------------------------------------

/// LLM client that uses the locally installed `codex` CLI as its backend.
///
/// Spawns `codex exec --json` as a subprocess for each LLM turn.  No API key
/// required — uses Codex's stored credentials.
///
/// ## Limitations
///
/// - **No structured tool calling** — Codex handles tools internally.
///   Tool events are informational only (displayed but not re-executed).
///
/// - **Stateless** — each `stream()` call spawns a fresh `codex` process.
///   Conversation history is formatted into the prompt.
///
/// - **Requires `codex` in PATH** — the CLI must be installed and
///   authenticated.
pub struct CodexClient {
    /// Path to the `codex` binary.
    codex_path: String,

    /// Workspace to expose as MCP tools to Codex.
    ///
    /// When `Some`, each call to `stream()` will start an in-process HTTP
    /// MCP server, register it with Codex via `codex mcp add`, and pass
    /// the workspace tools.  When `None`, no MCP server is started.
    workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,

    /// Whether sandbox enforcement is disabled.
    dangerous_no_sandbox: bool,
}

impl CodexClient {
    /// Create a new Codex CLI client.
    ///
    /// ## Parameters
    ///
    /// - `codex_path`: Path to the `codex` binary.  Pass `None` to auto-
    ///   resolve via `which codex`, falling back to bare `"codex"`.
    ///
    /// - `workspace`: If `Some`, the client will start an in-process HTTP
    ///   MCP server per `stream()` call, exposing workspace tools.
    ///
    /// - `dangerous_no_sandbox`: Whether `--dangerous-no-sandbox` was passed.
    ///   Forwarded to `McpHttpServer`.
    pub fn new(
        codex_path: Option<&str>,
        workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,
        dangerous_no_sandbox: bool,
    ) -> Self {
        let resolved = match codex_path {
            Some(p) => p.to_string(),
            None => super::resolve_binary_path("codex"),
        };

        Self {
            codex_path: resolved,
            workspace,
            dangerous_no_sandbox,
        }
    }
}

// ---------------------------------------------------------------------------
// LlmClient implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmClient for CodexClient {
    /// Codex runs its own agent loop with built-in tools (shell, file ops,
    /// MCP, web search).  Dyson should NOT re-execute those tool calls.
    fn handles_tools_internally(&self) -> bool {
        true
    }

    async fn stream(
        &self,
        messages: &[Message],
        system: &str,
        tools: &[ToolDefinition],
        config: &CompletionConfig,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        // Format conversation history into a single prompt string.
        let prompt = super::format_prompt(messages, tools);

        tracing::debug!(
            model = config.model,
            message_count = messages.len(),
            tool_count = tools.len(),
            prompt_len = prompt.len(),
            "spawning codex CLI"
        );

        // -- Start MCP server if workspace is available --
        //
        // Codex supports MCP via `codex mcp` subcommands, but for per-session
        // config we can't easily register servers dynamically.  Instead, we
        // pass MCP config via environment variable or config override.
        //
        // For now, workspace tools are injected via the developer_instructions
        // system prompt (similar to how Claude Code gets them via --mcp-config).
        // TODO: Use Codex's MCP support when stable.
        let mut _mcp_server_handle: Option<tokio::task::JoinHandle<()>> = None;
        let mut mcp_url: Option<String> = None;

        if let Some(ref workspace) = self.workspace {
            let server = Arc::new(McpHttpServer::new(
                Arc::clone(workspace),
                self.dangerous_no_sandbox,
            ));

            let (port, handle) = server.start().await.map_err(|e| {
                DysonError::Llm(format!("failed to start MCP HTTP server: {e}"))
            })?;

            tracing::info!(port = port, "MCP server started for Codex");
            mcp_url = Some(format!("http://127.0.0.1:{port}/mcp"));
            _mcp_server_handle = Some(handle);
        }

        // -- Build the command --
        let mut cmd = tokio::process::Command::new(&self.codex_path);
        cmd.arg("exec")
            .arg("--json")
            .arg("--ephemeral")
            .arg("--skip-git-repo-check");

        // Only bypass all approvals and sandboxing when explicitly requested
        // via --dangerous-no-sandbox.  Otherwise use --full-auto which keeps
        // Codex's workspace sandbox active but skips most approval prompts.
        if self.dangerous_no_sandbox {
            cmd.arg("--dangerously-bypass-approvals-and-sandbox");
        } else {
            cmd.arg("--full-auto");
        }

        cmd.arg("--model")
            .arg(&config.model);

        // Inject system prompt via developer_instructions config override.
        if !system.is_empty() {
            cmd.arg("-c").arg(format!("developer_instructions={system}"));
        }

        // If we have an MCP server running, tell Codex about it.
        // Codex doesn't have a --mcp-config CLI flag, so we register via
        // config override.  The config path is mcp_servers.<name>.url.
        if let Some(ref url) = mcp_url {
            cmd.arg("-c")
                .arg(format!("mcp_servers.dyson-workspace.url={url}"));
        }

        // The prompt goes as a positional argument.
        cmd.arg(&prompt)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        // -- Spawn the process --
        let mut child = cmd.spawn().map_err(|e| {
            DysonError::Llm(format!(
                "failed to spawn '{}': {e}.  Is Codex CLI installed?  \
                 Install with: npm install -g @openai/codex",
                self.codex_path
            ))
        })?;

        // -- Read stdout line by line and parse JSONL events --
        let stdout = child.stdout.take().ok_or_else(|| {
            DysonError::Llm("failed to open stdout for codex process".into())
        })?;

        let reader = BufReader::new(stdout);

        let event_stream = async_stream::stream! {
            let _child = child;
            let _mcp_handle = _mcp_server_handle;

            let mut reader = reader;
            let mut completed = false;

            loop {
                let mut line = String::new();
                let bytes_read = match reader.read_line(&mut line).await {
                    Ok(n) => n,
                    Err(e) => {
                        yield Err(DysonError::Io(e));
                        break;
                    }
                };
                if bytes_read == 0 {
                    break; // EOF
                }
                let line = line.trim_end().to_string();
                if line.is_empty() {
                    continue;
                }

                let json: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            line = line,
                            error = %e,
                            "failed to parse codex CLI JSON line"
                        );
                        continue;
                    }
                };

                let event_type = json["type"].as_str().unwrap_or("");

                match event_type {
                    // -------------------------------------------------
                    // thread.started — session initialized
                    // -------------------------------------------------
                    "thread.started" => {
                        if let Some(thread_id) = json["thread_id"].as_str() {
                            tracing::debug!(
                                thread_id = thread_id,
                                "codex CLI session started"
                            );
                        }
                    }

                    // -------------------------------------------------
                    // turn.started — new LLM turn begins
                    // -------------------------------------------------
                    "turn.started" => {
                        tracing::trace!("codex turn started");
                    }

                    // -------------------------------------------------
                    // turn.completed — turn finished successfully
                    // -------------------------------------------------
                    "turn.completed" => {
                        if let Some(usage) = json.get("usage") {
                            tracing::info!(
                                input_tokens = usage["input_tokens"].as_u64().unwrap_or(0),
                                output_tokens = usage["output_tokens"].as_u64().unwrap_or(0),
                                "codex turn completed"
                            );
                        }
                        if !completed {
                            completed = true;
                            yield Ok(StreamEvent::MessageComplete {
                                stop_reason: StopReason::EndTurn,
                            });
                        }
                    }

                    // -------------------------------------------------
                    // turn.failed — turn ended with error
                    // -------------------------------------------------
                    "turn.failed" => {
                        completed = true;
                        let error_msg = json["error"]["message"]
                            .as_str()
                            .unwrap_or("unknown error");
                        tracing::error!(error = error_msg, "codex turn failed");
                        yield Err(DysonError::Llm(
                            format!("Codex CLI error: {error_msg}")
                        ));
                    }

                    // -------------------------------------------------
                    // error — stream-level error
                    // -------------------------------------------------
                    "error" => {
                        let error_msg = json["message"]
                            .as_str()
                            .unwrap_or("unknown error");
                        tracing::error!(error = error_msg, "codex stream error");
                        yield Err(DysonError::Llm(
                            format!("Codex CLI error: {error_msg}")
                        ));
                    }

                    // -------------------------------------------------
                    // item.started — tool execution began
                    // -------------------------------------------------
                    "item.started" => {
                        let item = &json["item"];
                        let item_type = item["type"].as_str().unwrap_or("");

                        match item_type {
                            "command_execution" => {
                                let command = item["command"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                let id = item["id"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                yield Ok(StreamEvent::ToolUseStart {
                                    id,
                                    name: "bash".to_string(),
                                });
                                yield Ok(StreamEvent::ToolUseInputDelta(
                                    serde_json::json!({"command": command}).to_string()
                                ));
                            }
                            "mcp_tool_call" => {
                                let tool = item["tool"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                let id = item["id"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                yield Ok(StreamEvent::ToolUseStart {
                                    id,
                                    name: tool,
                                });
                            }
                            "web_search" => {
                                let id = item["id"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                yield Ok(StreamEvent::ToolUseStart {
                                    id,
                                    name: "web_search".to_string(),
                                });
                            }
                            _ => {}
                        }
                    }

                    // -------------------------------------------------
                    // item.completed — tool finished or agent message
                    // -------------------------------------------------
                    "item.completed" => {
                        let item = &json["item"];
                        let item_type = item["type"].as_str().unwrap_or("");

                        match item_type {
                            "agent_message" => {
                                if let Some(text) = item["text"].as_str() {
                                    if !text.is_empty() {
                                        yield Ok(StreamEvent::TextDelta(
                                            text.to_string()
                                        ));
                                    }
                                }
                            }
                            "reasoning" => {
                                if let Some(text) = item["text"].as_str() {
                                    if !text.is_empty() {
                                        yield Ok(StreamEvent::ThinkingDelta(
                                            text.to_string()
                                        ));
                                    }
                                }
                            }
                            "command_execution" => {
                                let id = item["id"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                let command = item["command"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                let output = item["aggregated_output"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                let exit_code = item["exit_code"].as_i64();

                                // Emit as a completed tool use so the UI
                                // shows what command was run and its output.
                                let input = serde_json::json!({
                                    "command": command,
                                    "output": output,
                                    "exit_code": exit_code,
                                });
                                yield Ok(StreamEvent::ToolUseComplete {
                                    id,
                                    name: "bash".to_string(),
                                    input,
                                });
                            }
                            "file_change" => {
                                let id = item["id"]
                                    .as_str()
                                    .unwrap_or("file_change")
                                    .to_string();
                                let changes = item["changes"].clone();
                                yield Ok(StreamEvent::ToolUseStart {
                                    id: id.clone(),
                                    name: "file_change".to_string(),
                                });
                                yield Ok(StreamEvent::ToolUseComplete {
                                    id,
                                    name: "file_change".to_string(),
                                    input: changes,
                                });
                            }
                            "mcp_tool_call" => {
                                let id = item["id"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                let tool = item["tool"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                let input = item["arguments"].clone();
                                yield Ok(StreamEvent::ToolUseComplete {
                                    id,
                                    name: tool,
                                    input,
                                });
                            }
                            "web_search" => {
                                let id = item["id"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                let query = item["query"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                yield Ok(StreamEvent::ToolUseComplete {
                                    id,
                                    name: "web_search".to_string(),
                                    input: serde_json::json!({"query": query}),
                                });
                            }
                            _ => {}
                        }
                    }

                    // item.updated — todo list updates, ignored
                    "item.updated" => {}

                    other => {
                        tracing::trace!(
                            event_type = other,
                            "unknown codex CLI event type"
                        );
                    }
                }
            }

            // If we never got a turn.completed event, emit an error.
            if !completed {
                tracing::error!("codex CLI exited without turn.completed event");
                yield Err(DysonError::Llm(
                    "Codex CLI process exited without producing a result".to_string()
                ));
            }
        };

        Ok(Box::pin(event_stream))
    }
}

// ---------------------------------------------------------------------------
// StreamParserState — testable line-parsing logic.
// ---------------------------------------------------------------------------

/// Mutable state for parsing Codex's JSONL output line by line.
///
/// Extracted from the `stream()` closure so we can unit test parsing
/// without spawning a subprocess.
#[cfg(test)]
struct StreamParserState {
    completed: bool,
}

#[cfg(test)]
impl StreamParserState {
    fn new() -> Self {
        Self { completed: false }
    }

    /// Parse one JSONL line. Returns events to yield (may be empty).
    fn parse_line(&mut self, line: &str) -> Vec<Result<StreamEvent>> {
        let mut events = Vec::new();

        let json: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return events,
        };

        let event_type = json["type"].as_str().unwrap_or("");

        match event_type {
            "turn.completed" => {
                if !self.completed {
                    self.completed = true;
                    events.push(Ok(StreamEvent::MessageComplete {
                        stop_reason: StopReason::EndTurn,
                    }));
                }
            }

            "turn.failed" => {
                self.completed = true;
                let error_msg = json["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown error");
                events.push(Err(DysonError::Llm(
                    format!("Codex CLI error: {error_msg}"),
                )));
            }

            "error" => {
                let error_msg = json["message"]
                    .as_str()
                    .unwrap_or("unknown error");
                events.push(Err(DysonError::Llm(
                    format!("Codex CLI error: {error_msg}"),
                )));
            }

            "item.started" => {
                let item = &json["item"];
                let item_type = item["type"].as_str().unwrap_or("");

                match item_type {
                    "command_execution" => {
                        let command = item["command"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        let id = item["id"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        events.push(Ok(StreamEvent::ToolUseStart {
                            id,
                            name: "bash".to_string(),
                        }));
                        events.push(Ok(StreamEvent::ToolUseInputDelta(
                            serde_json::json!({"command": command}).to_string(),
                        )));
                    }
                    "mcp_tool_call" => {
                        let tool = item["tool"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        let id = item["id"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        events.push(Ok(StreamEvent::ToolUseStart {
                            id,
                            name: tool,
                        }));
                    }
                    _ => {}
                }
            }

            "item.completed" => {
                let item = &json["item"];
                let item_type = item["type"].as_str().unwrap_or("");

                match item_type {
                    "agent_message" => {
                        if let Some(text) = item["text"].as_str() {
                            if !text.is_empty() {
                                events.push(Ok(StreamEvent::TextDelta(text.to_string())));
                            }
                        }
                    }
                    "reasoning" => {
                        if let Some(text) = item["text"].as_str() {
                            if !text.is_empty() {
                                events.push(Ok(StreamEvent::ThinkingDelta(text.to_string())));
                            }
                        }
                    }
                    "command_execution" => {
                        let id = item["id"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        let command = item["command"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        let output = item["aggregated_output"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        let exit_code = item["exit_code"].as_i64();
                        let input = serde_json::json!({
                            "command": command,
                            "output": output,
                            "exit_code": exit_code,
                        });
                        events.push(Ok(StreamEvent::ToolUseComplete {
                            id,
                            name: "bash".to_string(),
                            input,
                        }));
                    }
                    "mcp_tool_call" => {
                        let id = item["id"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        let tool = item["tool"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        let input = item["arguments"].clone();
                        events.push(Ok(StreamEvent::ToolUseComplete {
                            id,
                            name: tool,
                            input,
                        }));
                    }
                    _ => {}
                }
            }

            _ => {}
        }

        events
    }

    /// Called after EOF.
    fn finalize(&mut self) -> Vec<Result<StreamEvent>> {
        let mut events = Vec::new();
        if !self.completed {
            events.push(Err(DysonError::Llm(
                "Codex CLI process exited without producing a result".to_string(),
            )));
        }
        events
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // JSONL event parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_thread_started() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"type":"thread.started","thread_id":"test-123"}"#,
        )
        .unwrap();
        assert_eq!(json["type"].as_str().unwrap(), "thread.started");
        assert_eq!(json["thread_id"].as_str().unwrap(), "test-123");
    }

    #[test]
    fn turn_completed_yields_message_complete() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":50}}"#,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(StreamEvent::MessageComplete { stop_reason }) => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
            }
            other => panic!("expected Ok(MessageComplete), got: {other:?}"),
        }
    }

    #[test]
    fn turn_failed_yields_error() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"turn.failed","error":{"message":"Rate limit exceeded"}}"#,
        );
        assert_eq!(events.len(), 1);
        assert!(events[0].is_err());
        let err_msg = format!("{}", events[0].as_ref().unwrap_err());
        assert!(err_msg.contains("Rate limit exceeded"));
    }

    #[test]
    fn stream_error_yields_error() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"error","message":"Auth token expired"}"#,
        );
        assert_eq!(events.len(), 1);
        assert!(events[0].is_err());
        let err_msg = format!("{}", events[0].as_ref().unwrap_err());
        assert!(err_msg.contains("Auth token expired"));
    }

    #[test]
    fn agent_message_yields_text_delta() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"item.completed","item":{"id":"msg_1","type":"agent_message","text":"Hello world"}}"#,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(StreamEvent::TextDelta(text)) => assert_eq!(text, "Hello world"),
            other => panic!("expected TextDelta, got: {other:?}"),
        }
    }

    #[test]
    fn reasoning_yields_thinking_delta() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"item.completed","item":{"id":"r_1","type":"reasoning","text":"Let me think..."}}"#,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(StreamEvent::ThinkingDelta(text)) => assert_eq!(text, "Let me think..."),
            other => panic!("expected ThinkingDelta, got: {other:?}"),
        }
    }

    #[test]
    fn command_started_yields_tool_use_start() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"item.started","item":{"id":"cmd_1","type":"command_execution","command":"ls -la","status":"in_progress"}}"#,
        );
        assert_eq!(events.len(), 2);
        match &events[0] {
            Ok(StreamEvent::ToolUseStart { id, name }) => {
                assert_eq!(id, "cmd_1");
                assert_eq!(name, "bash");
            }
            other => panic!("expected ToolUseStart, got: {other:?}"),
        }
        match &events[1] {
            Ok(StreamEvent::ToolUseInputDelta(delta)) => {
                assert!(delta.contains("ls -la"));
            }
            other => panic!("expected ToolUseInputDelta, got: {other:?}"),
        }
    }

    #[test]
    fn command_completed_yields_tool_use_complete() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"item.completed","item":{"id":"cmd_1","type":"command_execution","command":"ls","aggregated_output":"Cargo.toml\nsrc/","exit_code":0,"status":"completed"}}"#,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(StreamEvent::ToolUseComplete { id, name, input }) => {
                assert_eq!(id, "cmd_1");
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "ls");
                assert_eq!(input["output"], "Cargo.toml\nsrc/");
                assert_eq!(input["exit_code"], 0);
            }
            other => panic!("expected ToolUseComplete, got: {other:?}"),
        }
    }

    #[test]
    fn mcp_tool_call_started_yields_tool_use_start() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"item.started","item":{"id":"mcp_1","type":"mcp_tool_call","server":"dyson-workspace","tool":"workspace_view","arguments":{},"status":"in_progress"}}"#,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(StreamEvent::ToolUseStart { id, name }) => {
                assert_eq!(id, "mcp_1");
                assert_eq!(name, "workspace_view");
            }
            other => panic!("expected ToolUseStart, got: {other:?}"),
        }
    }

    #[test]
    fn mcp_tool_call_completed_yields_tool_use_complete() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"item.completed","item":{"id":"mcp_1","type":"mcp_tool_call","server":"dyson-workspace","tool":"workspace_view","arguments":{"key":"SOUL"},"result":{"content":[]},"status":"completed"}}"#,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(StreamEvent::ToolUseComplete { id, name, input }) => {
                assert_eq!(id, "mcp_1");
                assert_eq!(name, "workspace_view");
                assert_eq!(input["key"], "SOUL");
            }
            other => panic!("expected ToolUseComplete, got: {other:?}"),
        }
    }

    #[test]
    fn no_turn_completed_yields_error_on_finalize() {
        let mut state = StreamParserState::new();
        state.parse_line(
            r#"{"type":"item.completed","item":{"id":"msg_1","type":"agent_message","text":"Hi"}}"#,
        );
        let final_events = state.finalize();
        assert_eq!(final_events.len(), 1);
        assert!(final_events[0].is_err());
    }

    #[test]
    fn finalize_after_turn_completed_produces_nothing() {
        let mut state = StreamParserState::new();
        state.parse_line(
            r#"{"type":"turn.completed","usage":{"input_tokens":0,"output_tokens":0}}"#,
        );
        let final_events = state.finalize();
        assert!(final_events.is_empty());
    }

    #[test]
    fn empty_agent_message_is_skipped() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"item.completed","item":{"id":"msg_1","type":"agent_message","text":""}}"#,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn duplicate_turn_completed_ignored() {
        let mut state = StreamParserState::new();
        let events1 = state.parse_line(
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":50}}"#,
        );
        assert_eq!(events1.len(), 1);
        let events2 = state.parse_line(
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":50}}"#,
        );
        assert!(events2.is_empty(), "duplicate turn.completed should be ignored");
    }

    #[test]
    fn unknown_item_type_ignored() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"item.completed","item":{"id":"x","type":"todo_list","items":[]}}"#,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn invalid_json_ignored() {
        let mut state = StreamParserState::new();
        let events = state.parse_line("not valid json at all");
        assert!(events.is_empty());
    }
}
