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
//       --sandbox workspace-write \       (or --dangerously-bypass-approvals-and-sandbox)
//       --ephemeral \
//       --skip-git-repo-check \
//       --model <model> \
//       --profile <transient-profile>
//
//   The key flags:
//     exec                                Non-interactive mode
//     --json                              Emit JSONL events to stdout
//     --sandbox workspace-write           Keep the sandbox, allow workspace writes
//     --dangerously-bypass-approvals-and-sandbox
//                                         Only when --dangerous-no-sandbox is set
//     --ephemeral                         Don't persist session files
//     --skip-git-repo-check               Don't require a git repo
//     --model                             Model selection
//     --profile                          Load system prompt + MCP config from disk
//     stdin                              Carry the user prompt out-of-band
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
//   a single prompt string using the shared `format_prompt()` utility
//   in `llm/mod.rs`.
// ===========================================================================

use std::io::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt as _;

use crate::error::{DysonError, Result};
use crate::llm::cli_subprocess::{self, CliLineParser, cli_event_stream};
use crate::llm::stream::{StopReason, StreamEvent};
use crate::llm::{CompletionConfig, LlmClient, ToolDefinition};
use crate::message::Message;
use crate::tool::Tool;
use crate::workspace::WorkspaceHandle;

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
    /// MCP server and register it with Codex via `-c mcp_servers...` config
    /// override.  When `None`, no MCP server is started.
    workspace: Option<WorkspaceHandle>,

    /// Whether sandbox enforcement is disabled.
    dangerous_no_sandbox: bool,

    /// Dyson's agent tools to expose via MCP alongside workspace tools.
    mcp_tools: std::sync::Mutex<std::collections::HashMap<String, Arc<dyn Tool>>>,
}

struct TempCodexProfile {
    _file: tempfile::NamedTempFile,
    name: String,
}

impl TempCodexProfile {
    fn new(system: &str, mcp: Option<(&str, &str)>) -> Result<Self> {
        let home = codex_home_dir()?;
        std::fs::create_dir_all(&home).map_err(DysonError::Io)?;
        let mut file = tempfile::Builder::new()
            .prefix("dyson-mcp-")
            .suffix(".config.toml")
            .tempfile_in(&home)
            .map_err(DysonError::Io)?;
        let body = codex_profile_body(system, mcp);
        file.write_all(body.as_bytes()).map_err(DysonError::Io)?;
        file.flush().map_err(DysonError::Io)?;
        let filename = file
            .path()
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| DysonError::Llm("failed to build Codex MCP profile name".into()))?;
        let name = filename
            .strip_suffix(".config.toml")
            .ok_or_else(|| DysonError::Llm("unexpected Codex MCP profile suffix".into()))?
            .to_owned();
        Ok(Self { _file: file, name })
    }

    fn name(&self) -> &str {
        &self.name
    }
}

fn codex_profile_body(system: &str, mcp: Option<(&str, &str)>) -> String {
    // Keep large developer instructions off argv: a mature Dyson workspace can
    // exceed Linux's ARG_MAX before Codex even starts. JSON string escaping is
    // also valid for TOML basic strings and handles control characters.
    let system = serde_json::to_string(system).expect("serializing a string cannot fail");
    let mut body = format!("developer_instructions = {system}\n");

    if let Some((token, url)) = mcp {
        // Codex validates every config layer before merging later CLI
        // overrides. The profile must therefore contain a complete transport.
        body.push_str(&format!(
            concat!(
                "\n[mcp_servers.dyson-workspace]\n",
                "url = \"{}\"\n",
                "required = true\n",
                "default_tools_approval_mode = \"approve\"\n",
                "\n[mcp_servers.dyson-workspace.http_headers]\n",
                "Authorization = \"Bearer {}\"\n",
            ),
            toml_escape(url),
            toml_escape(token),
        ));
    }

    body
}

fn codex_home_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        DysonError::Llm("HOME is not set; cannot create Codex MCP profile".into())
    })?;
    Ok(PathBuf::from(home).join(".codex"))
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
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
        workspace: Option<WorkspaceHandle>,
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
            mcp_tools: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Build the CLI arguments for `codex exec`.
    ///
    /// Extracted as a method so the sandbox-gating logic is unit-testable
    /// without spawning a subprocess.
    fn build_args(&self, model: &str, mcp_profile: Option<&str>) -> Vec<String> {
        let mut args = vec![
            "exec".to_string(),
            "--json".to_string(),
            "--ephemeral".to_string(),
            "--skip-git-repo-check".to_string(),
        ];

        // Only bypass all approvals and sandboxing when explicitly requested
        // via --dangerous-no-sandbox.  Otherwise keep Codex's workspace
        // sandbox active with `--sandbox workspace-write` (the non-deprecated
        // replacement for `--full-auto`; in exec mode approvals are `never`
        // regardless, so this grants write access without prompting).
        if self.dangerous_no_sandbox {
            args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
        } else {
            args.push("--sandbox".to_string());
            args.push("workspace-write".to_string());
        }

        args.push("--model".to_string());
        args.push(model.to_string());

        if let Some(profile) = mcp_profile {
            args.push("--profile".to_string());
            args.push(profile.to_string());
        }

        args
    }
}

// ---------------------------------------------------------------------------
// LlmClient implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmClient for CodexClient {
    async fn stream(
        &self,
        messages: &[Message],
        system: &str,
        system_suffix: &str,
        tools: &[ToolDefinition],
        config: &CompletionConfig,
    ) -> Result<crate::llm::StreamResponse> {
        // When MCP is active, tools are structured — skip text descriptions.
        let prompt_tools = cli_subprocess::filter_tools_for_cli(tools, self.workspace.is_some());
        let prompt = super::format_prompt(messages, &prompt_tools);

        tracing::debug!(
            model = config.model,
            message_count = messages.len(),
            tool_count = tools.len(),
            prompt_len = prompt.len(),
            "spawning codex CLI"
        );

        // -- Start MCP server if workspace is available --
        let mut _mcp_server_handle: Option<tokio::task::JoinHandle<()>> = None;
        let mut mcp_url: Option<String> = None;
        let mut mcp_token: Option<String> = None;

        let extra = self
            .mcp_tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let server_info = if let Some(ref workspace) = self.workspace {
            Some(super::start_mcp_server(workspace, extra).await?)
        } else if !extra.is_empty() {
            Some(super::start_mcp_tools_server(extra).await?)
        } else {
            None
        };
        if let Some(info) = server_info {
            tracing::info!(port = info.port, "MCP server started for Codex");
            mcp_token = Some(info.token);
            mcp_url = Some(info.url);
            _mcp_server_handle = Some(info.handle);
        }

        // -- Build the command --
        let full_system = super::concat_system_prompt(system, system_suffix);
        let mcp = mcp_token.as_deref().zip(mcp_url.as_deref());
        let mcp_profile = TempCodexProfile::new(&full_system, mcp)?;
        let args = self.build_args(&config.model, Some(mcp_profile.name()));

        let mut cmd = tokio::process::Command::new(&self.codex_path);
        for arg in &args {
            cmd.arg(arg);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // The child lives inside the stream's keep_alive bag; when a
            // cancelled turn drops the stream, kill the CLI instead of
            // orphaning it (it would otherwise keep burning tokens).
            .kill_on_drop(true)
            .env_clear()
            .envs(cli_subprocess::sanitized_child_env(std::env::vars()));

        // -- Spawn the process --
        let mut child = cmd.spawn().map_err(|e| {
            DysonError::Llm(format!(
                "failed to spawn '{}': {e}.  Is Codex CLI installed?  \
                 Install with: npm install -g @openai/codex",
                self.codex_path
            ))
        })?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| DysonError::Llm("failed to open stdin for codex process".into()))?;
        stdin
            .write_all(prompt.as_bytes())
            .await
            .map_err(DysonError::Io)?;
        stdin.write_all(b"\n").await.map_err(DysonError::Io)?;
        stdin.shutdown().await.map_err(DysonError::Io)?;
        drop(stdin);

        // -- Read stdout line by line and parse JSONL events --
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DysonError::Llm("failed to open stdout for codex process".into()))?;

        // Keep child process and MCP server alive for the stream's lifetime.
        let mut keep_alive: Vec<Box<dyn std::any::Any + Send>> = vec![Box::new(child)];
        if let Some(handle) = _mcp_server_handle {
            keep_alive.push(Box::new(handle));
        }
        keep_alive.push(Box::new(mcp_profile));

        let event_stream = cli_event_stream(stdout, StreamParserState::new(), keep_alive);

        Ok(cli_subprocess::build_observe_response(event_stream))
    }

    fn set_mcp_tools(&self, tools: std::collections::HashMap<String, Arc<dyn Tool>>) {
        let filtered: std::collections::HashMap<_, _> =
            tools.into_iter().filter(|(_, t)| !t.agent_only()).collect();
        tracing::info!(tool_count = filtered.len(), "MCP tools registered");
        *self
            .mcp_tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = filtered;
    }
}

// ---------------------------------------------------------------------------
// StreamParserState — testable line-parsing logic.
// ---------------------------------------------------------------------------

/// Mutable state for parsing Codex's JSONL output line by line.
///
/// This is the single source of truth for Codex event parsing.  Used by
/// both the `stream()` async closure (production) and unit tests.
struct StreamParserState {
    completed: bool,
}

impl StreamParserState {
    const fn new() -> Self {
        Self { completed: false }
    }
}

impl CliLineParser for StreamParserState {
    fn parse_line(&mut self, line: &str) -> Vec<Result<StreamEvent>> {
        let mut events = Vec::new();

        let json: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                if !line.is_empty() {
                    tracing::warn!(error = %e, line_prefix = &line[..line.len().min(120)], "Codex CLI: malformed JSONL line");
                }
                return events;
            }
        };

        let event_type = json["type"].as_str().unwrap_or("");

        match event_type {
            "turn.completed" if !self.completed => {
                self.completed = true;
                events.push(Ok(StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    output_tokens: None,
                }));
            }

            "turn.failed" => {
                self.completed = true;
                let error_msg = json["error"]["message"].as_str().unwrap_or("unknown error");
                events.push(Err(DysonError::Llm(format!(
                    "Codex CLI error: {error_msg}"
                ))));
            }

            "error" => {
                let error_msg = json["message"].as_str().unwrap_or("unknown error");
                events.push(Err(DysonError::Llm(format!(
                    "Codex CLI error: {error_msg}"
                ))));
            }

            "item.started" => {
                let item = &json["item"];
                let item_type = item["type"].as_str().unwrap_or("");

                match item_type {
                    "command_execution" => {
                        let command = item["command"].as_str().unwrap_or("").to_string();
                        let id = item["id"].as_str().unwrap_or("").to_string();
                        events.push(Ok(StreamEvent::ToolUseStart {
                            id,
                            name: "bash".to_string(),
                        }));
                        events.push(Ok(StreamEvent::ToolUseInputDelta(
                            serde_json::json!({"command": command}).to_string(),
                        )));
                    }
                    "mcp_tool_call" => {
                        let tool = item["tool"].as_str().unwrap_or("").to_string();
                        let id = item["id"].as_str().unwrap_or("").to_string();
                        events.push(Ok(StreamEvent::ToolUseStart { id, name: tool }));
                    }
                    "web_search" => {
                        let id = item["id"].as_str().unwrap_or("").to_string();
                        events.push(Ok(StreamEvent::ToolUseStart {
                            id,
                            name: "web_search".to_string(),
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
                        if let Some(text) = item["text"].as_str()
                            && !text.is_empty()
                        {
                            events.push(Ok(StreamEvent::TextDelta(text.to_string())));
                        }
                    }
                    "reasoning" => {
                        if let Some(text) = item["text"].as_str()
                            && !text.is_empty()
                        {
                            events.push(Ok(StreamEvent::ThinkingDelta(text.to_string())));
                        }
                    }
                    "command_execution" => {
                        let id = item["id"].as_str().unwrap_or("").to_string();
                        let command = item["command"].as_str().unwrap_or("").to_string();
                        let output = item["aggregated_output"].as_str().unwrap_or("").to_string();
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
                    "file_change" => {
                        let id = item["id"].as_str().unwrap_or("file_change").to_string();
                        let changes = item["changes"].clone();
                        events.push(Ok(StreamEvent::ToolUseStart {
                            id: id.clone(),
                            name: "file_change".to_string(),
                        }));
                        events.push(Ok(StreamEvent::ToolUseComplete {
                            id,
                            name: "file_change".to_string(),
                            input: changes,
                        }));
                    }
                    "mcp_tool_call" => {
                        let id = item["id"].as_str().unwrap_or("").to_string();
                        let tool = item["tool"].as_str().unwrap_or("").to_string();
                        let input = item["arguments"].clone();
                        events.push(Ok(StreamEvent::ToolUseComplete {
                            id,
                            name: tool,
                            input,
                        }));
                    }
                    "web_search" => {
                        let id = item["id"].as_str().unwrap_or("").to_string();
                        let query = item["query"].as_str().unwrap_or("").to_string();
                        events.push(Ok(StreamEvent::ToolUseComplete {
                            id,
                            name: "web_search".to_string(),
                            input: serde_json::json!({"query": query}),
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
        let json: serde_json::Value =
            serde_json::from_str(r#"{"type":"thread.started","thread_id":"test-123"}"#).unwrap();
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
            Ok(StreamEvent::MessageComplete { stop_reason, .. }) => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
            }
            other => panic!("expected Ok(MessageComplete), got: {other:?}"),
        }
    }

    #[test]
    fn turn_failed_yields_error() {
        let mut state = StreamParserState::new();
        let events =
            state.parse_line(r#"{"type":"turn.failed","error":{"message":"Rate limit exceeded"}}"#);
        assert_eq!(events.len(), 1);
        assert!(events[0].is_err());
        let err_msg = format!("{}", events[0].as_ref().unwrap_err());
        assert!(err_msg.contains("Rate limit exceeded"));
    }

    #[test]
    fn stream_error_yields_error() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(r#"{"type":"error","message":"Auth token expired"}"#);
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
            r#"{"type":"item.started","item":{"id":"mcp_1","type":"mcp_tool_call","server":"dyson-workspace","tool":"workspace","arguments":{},"status":"in_progress"}}"#,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(StreamEvent::ToolUseStart { id, name }) => {
                assert_eq!(id, "mcp_1");
                assert_eq!(name, "workspace");
            }
            other => panic!("expected ToolUseStart, got: {other:?}"),
        }
    }

    #[test]
    fn mcp_tool_call_completed_yields_tool_use_complete() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"item.completed","item":{"id":"mcp_1","type":"mcp_tool_call","server":"dyson-workspace","tool":"workspace","arguments":{"key":"SOUL"},"result":{"content":[]},"status":"completed"}}"#,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(StreamEvent::ToolUseComplete { id, name, input }) => {
                assert_eq!(id, "mcp_1");
                assert_eq!(name, "workspace");
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
        assert!(
            events2.is_empty(),
            "duplicate turn.completed should be ignored"
        );
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

    #[test]
    fn empty_line_returns_no_events() {
        let mut state = StreamParserState::new();
        let events = state.parse_line("");
        assert!(events.is_empty());
    }

    #[test]
    fn web_search_started_yields_tool_use_start() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"item.started","item":{"id":"ws_1","type":"web_search","query":"rust async","status":"in_progress"}}"#,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(StreamEvent::ToolUseStart { id, name }) => {
                assert_eq!(id, "ws_1");
                assert_eq!(name, "web_search");
            }
            other => panic!("expected ToolUseStart, got: {other:?}"),
        }
    }

    #[test]
    fn web_search_completed_yields_tool_use_complete() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"item.completed","item":{"id":"ws_1","type":"web_search","query":"rust async","results":[],"status":"completed"}}"#,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(StreamEvent::ToolUseComplete { id, name, input }) => {
                assert_eq!(id, "ws_1");
                assert_eq!(name, "web_search");
                assert_eq!(input["query"], "rust async");
            }
            other => panic!("expected ToolUseComplete, got: {other:?}"),
        }
    }

    #[test]
    fn file_change_completed_yields_start_and_complete() {
        let mut state = StreamParserState::new();
        let events = state.parse_line(
            r#"{"type":"item.completed","item":{"id":"fc_1","type":"file_change","changes":[{"path":"src/main.rs","action":"edit"}],"status":"completed"}}"#,
        );
        assert_eq!(events.len(), 2);
        match &events[0] {
            Ok(StreamEvent::ToolUseStart { id, name }) => {
                assert_eq!(id, "fc_1");
                assert_eq!(name, "file_change");
            }
            other => panic!("expected ToolUseStart, got: {other:?}"),
        }
        match &events[1] {
            Ok(StreamEvent::ToolUseComplete { id, name, input }) => {
                assert_eq!(id, "fc_1");
                assert_eq!(name, "file_change");
                assert!(input.is_array());
            }
            other => panic!("expected ToolUseComplete, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // build_args tests
    // -----------------------------------------------------------------------

    #[test]
    fn build_args_uses_workspace_write_sandbox_by_default() {
        let client = CodexClient::new(Some("codex"), None, false);
        let args = client.build_args("o3", None);
        // `--sandbox workspace-write` is the non-deprecated replacement for
        // the old `--full-auto`; the value follows the flag as a separate arg.
        let i = args
            .iter()
            .position(|a| a == "--sandbox")
            .expect("should pass --sandbox when sandbox is enabled");
        assert_eq!(args.get(i + 1).map(String::as_str), Some("workspace-write"));
        assert!(
            !args.contains(&"--full-auto".to_string()),
            "should not use the deprecated --full-auto flag"
        );
        assert!(
            !args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()),
            "should NOT bypass sandbox when flag is not set"
        );
    }

    #[test]
    fn build_args_bypasses_sandbox_when_flag_set() {
        let client = CodexClient::new(Some("codex"), None, true);
        let args = client.build_args("o3", None);
        assert!(
            args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()),
            "should bypass sandbox when --dangerous-no-sandbox is set"
        );
        assert!(
            !args.contains(&"--full-auto".to_string()),
            "should NOT use --full-auto when bypassing sandbox"
        );
    }

    #[test]
    fn build_args_includes_model() {
        let client = CodexClient::new(Some("codex"), None, false);
        let args = client.build_args("o4-mini", None);
        assert!(args.contains(&"o4-mini".to_string()));
    }

    #[test]
    fn build_args_keeps_system_prompt_user_prompt_and_mcp_config_off_argv() {
        let client = CodexClient::new(Some("codex"), None, false);
        let args = client.build_args("o3", Some("dyson-mcp-random-profile"));
        assert!(
            !args.iter().any(|a| a.contains("developer_instructions")
                || a.contains("Authorization")
                || a.contains("http://127.0.0.1")),
            "large or secret request material must not be placed on argv: {args:?}"
        );
        assert_eq!(
            args.iter()
                .position(|arg| arg == "--profile")
                .and_then(|index| args.get(index + 1))
                .map(String::as_str),
            Some("dyson-mcp-random-profile")
        );
    }

    #[test]
    fn profile_round_trips_large_developer_instructions_and_complete_mcp_transport() {
        let system = "Dyson line one\nDyson line two\twith controls".repeat(16_384);
        let body = codex_profile_body(
            &system,
            Some(("secret-token-123", "http://127.0.0.1:9999/mcp")),
        );
        let parsed: toml::Value = toml::from_str(&body).expect("valid transient Codex profile");
        assert_eq!(
            parsed["developer_instructions"].as_str(),
            Some(system.as_str())
        );
        assert_eq!(
            parsed["mcp_servers"]["dyson-workspace"]["url"].as_str(),
            Some("http://127.0.0.1:9999/mcp")
        );
        assert_eq!(
            parsed["mcp_servers"]["dyson-workspace"]["default_tools_approval_mode"].as_str(),
            Some("approve")
        );
        assert_eq!(
            parsed["mcp_servers"]["dyson-workspace"]["http_headers"]["Authorization"].as_str(),
            Some("Bearer secret-token-123")
        );
    }

    #[test]
    fn build_args_has_no_positional_prompt() {
        let client = CodexClient::new(Some("codex"), None, false);
        let args = client.build_args("o3", None);
        assert!(
            !args.iter().any(|arg| arg == "-" || arg == "--"),
            "with piped stdin Codex reads the prompt when no positional prompt is present: {args:?}"
        );
    }

    struct LiveAgentsListTool;

    #[async_trait::async_trait]
    impl Tool for LiveAgentsListTool {
        fn name(&self) -> &str {
            "agents.list"
        }

        fn description(&self) -> &str {
            "List managed agents"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }

        async fn run(
            &self,
            _input: &serde_json::Value,
            _ctx: &crate::tool::ToolContext,
        ) -> crate::error::Result<crate::tool::ToolOutput> {
            Ok(crate::tool::ToolOutput::success(
                "DYSON_CODEX_FORWARDED_MCP_LIVE_OK",
            ))
        }
    }

    /// Live regression for the complete Codex CLI -> loopback MCP -> Dyson
    /// workspace path. Deliberately ignored because it consumes a signed-in
    /// Codex turn; run explicitly while validating managed-image upgrades.
    #[tokio::test]
    #[ignore = "requires an installed, signed-in Codex CLI"]
    async fn live_codex_exec_calls_workspace_mcp() {
        use std::collections::HashMap;
        use std::sync::Arc;

        use tokio::sync::RwLock;

        let workspace: crate::workspace::WorkspaceHandle = Arc::new(RwLock::new(Box::new(
            crate::workspace::InMemoryWorkspace::new()
                .with_file("USER.md", "DYSON_CODEX_MCP_LIVE_OK"),
        )));
        let info = crate::llm::start_mcp_server(&workspace, HashMap::new())
            .await
            .expect("start live MCP server");
        let profile = TempCodexProfile::new(
            "Use the requested MCP tool. Do not answer from memory.",
            Some((&info.token, &info.url)),
        )
        .expect("create transient MCP profile");
        let client = CodexClient::new(Some("codex"), Some(workspace), false);
        let args = client.build_args("gpt-5.6-sol", Some(profile.name()));
        let mut child = tokio::process::Command::new("codex")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch Codex CLI");
        let mut stdin = child.stdin.take().expect("open Codex stdin");
        stdin
            .write_all(b"Call the dyson-workspace workspace tool with op=view and file=USER.md, then reply with exactly its contents.\n")
            .await
            .expect("write Codex prompt");
        stdin.shutdown().await.expect("close Codex stdin");
        drop(stdin);
        let output = child.wait_with_output().await.expect("wait for Codex CLI");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("codex stdout:\n{stdout}");
        eprintln!("codex stderr:\n{stderr}");
        assert!(output.status.success(), "Codex CLI failed: {stderr}");
        assert!(
            stdout.contains("\"server\":\"dyson-workspace\"")
                && stdout.contains("\"tool\":\"workspace\""),
            "Codex never called the Dyson workspace MCP server: {stdout}"
        );
        assert!(
            stdout.contains("DYSON_CODEX_MCP_LIVE_OK"),
            "Codex did not return the workspace marker: {stdout}"
        );

        info.handle.abort();
    }

    /// Live regression for the HTTP/Swarm construction path, where Codex has
    /// forwarded tools but no workspace handle in the shared client registry.
    #[tokio::test]
    #[ignore = "requires an installed, signed-in Codex CLI"]
    async fn live_codex_exec_calls_forwarded_mcp_without_workspace() {
        use std::collections::HashMap;
        use std::sync::Arc;

        let mut tools = HashMap::new();
        tools.insert(
            "agents_list".to_string(),
            Arc::new(LiveAgentsListTool) as Arc<dyn Tool>,
        );
        let info = crate::llm::start_mcp_tools_server(tools)
            .await
            .expect("start forwarded-tools MCP server");
        let profile = TempCodexProfile::new(
            "Use the requested MCP tool. Do not answer from memory.",
            Some((&info.token, &info.url)),
        )
        .expect("create transient Codex profile");
        let client = CodexClient::new(Some("codex"), None, false);
        let args = client.build_args("gpt-5.6-sol", Some(profile.name()));
        let mut child = tokio::process::Command::new("codex")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch Codex CLI");
        let mut stdin = child.stdin.take().expect("open Codex stdin");
        stdin
            .write_all(b"Call the dyson-workspace agents_list tool with an empty object, then reply with exactly its result.\n")
            .await
            .expect("write Codex prompt");
        stdin.shutdown().await.expect("close Codex stdin");
        drop(stdin);
        let output = child.wait_with_output().await.expect("wait for Codex CLI");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("codex stdout:\n{stdout}");
        eprintln!("codex stderr:\n{stderr}");
        assert!(output.status.success(), "Codex CLI failed: {stderr}");
        assert!(
            stdout.contains("\"server\":\"dyson-workspace\"")
                && stdout.contains("\"tool\":\"agents_list\""),
            "Codex never called the forwarded MCP tool: {stdout}"
        );
        assert!(
            stdout.contains("DYSON_CODEX_FORWARDED_MCP_LIVE_OK"),
            "Codex did not return the forwarded-tool marker: {stdout}"
        );

        info.handle.abort();
    }
}
