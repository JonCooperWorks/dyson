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
//       --tools "" \
//       --model <model> \
//       --system-prompt <system>
//
//   The key flags:
//     -p                          Print mode (non-interactive, pipe-friendly)
//     --output-format stream-json Emit newline-delimited JSON events
//     --verbose                   Required for stream-json output
//     --include-partial-messages  Emit raw Anthropic streaming events
//                                 (content_block_delta, etc.) for true
//                                 token-by-token streaming
//     --dangerously-skip-permissions  Let Claude Code run tools without
//                                     prompting (Dyson is non-interactive)
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
//   transport layer (terminal, Telegram, etc.).  Claude Code handles
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
// ===========================================================================

use std::collections::HashMap;
use std::pin::Pin;
use std::process::Stdio;

use async_trait::async_trait;
use futures::Stream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::error::{DysonError, Result};
use crate::llm::stream::{StopReason, StreamEvent};
use crate::llm::{CompletionConfig, LlmClient, ToolDefinition};
use crate::message::{ContentBlock, Message, Role};

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
}

impl ClaudeCodeClient {
    /// Create a new Claude Code client.
    ///
    /// `claude_path` is optional — pass `None` to resolve automatically.
    /// Resolution order: explicit path > `which claude` > fallback "claude".
    ///
    /// We resolve the absolute path at startup so that service environments
    /// (systemd, launchd) work even with a minimal PATH.
    /// `mcp_configs` is a list of MCP server JSON configs to forward.
    pub fn new(claude_path: Option<&str>, mcp_configs: Vec<String>) -> Self {
        let resolved = match claude_path {
            Some(p) => p.to_string(),
            None => resolve_claude_path(),
        };

        Self {
            claude_path: resolved,
            mcp_configs,
        }
    }
}

/// Resolve the absolute path to the `claude` binary.
///
/// Uses `which claude` to find it on the current PATH.  This is important
/// for service environments (systemd, launchd) where PATH is minimal and
/// won't include npm global bin directories.  By resolving at startup
/// (which happens before daemonizing or during the first run), we capture
/// the full path while the user's PATH is still available.
fn resolve_claude_path() -> String {
    std::process::Command::new("which")
        .arg("claude")
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    tracing::info!(path = path, "resolved claude binary path");
                    return Some(path);
                }
            }
            None
        })
        .unwrap_or_else(|| {
            tracing::warn!("could not resolve claude path — falling back to 'claude'");
            "claude".to_string()
        })
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
    /// | `--tools ""` | Disable Claude Code's built-in tools |
    /// | `--model <model>` | Model selection |
    /// | `--system-prompt <prompt>` | System prompt |
    /// | `--no-session-persistence` | Don't save this to Claude Code's history |
    /// Claude Code runs its own agent loop with built-in tools (Bash, Read,
    /// Write, etc.).  Dyson should NOT re-execute those tool calls — they
    /// already ran inside the `claude -p` subprocess.  ToolUse stream events
    /// are still emitted for display purposes (so the user sees what Claude
    /// Code is doing), but the agent loop skips execution and breaks after
    /// a single iteration.
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
        // -- Format conversation history into a prompt string --
        //
        // The claude CLI in -p mode takes a single prompt.  We format the
        // entire conversation history into a readable string so the model
        // has context from previous turns.
        //
        // For single-turn conversations (most common), this is just the
        // user's message.  For multi-turn, we include the full history.
        let prompt = format_prompt(messages, tools);

        tracing::debug!(
            model = config.model,
            message_count = messages.len(),
            tool_count = tools.len(),
            prompt_len = prompt.len(),
            "spawning claude CLI"
        );

        // -- Build the command --
        let mut cmd = tokio::process::Command::new(&self.claude_path);
        cmd.arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--include-partial-messages")
            .arg("--no-session-persistence")
            .arg("--dangerously-skip-permissions")
            .arg("--model")
            .arg(&config.model)
            .arg("--append-system-prompt")
            .arg(system)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        // Forward MCP server configs if any.
        for mcp_json in &self.mcp_configs {
            cmd.arg("--mcp-config").arg(mcp_json);
        }

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
        let mut stdin = child.stdin.take().ok_or_else(|| {
            DysonError::Llm("failed to open stdin for claude process".into())
        })?;

        // Write in a spawned task so we don't block the stream setup.
        // For large conversation histories, this could take a moment.
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
            let _ = stdin.flush().await;
            // stdin is dropped here, closing the pipe.
        });

        // -- Read stdout line by line and parse JSON events --
        let stdout = child.stdout.take().ok_or_else(|| {
            DysonError::Llm("failed to open stdout for claude process".into())
        })?;

        let reader = BufReader::new(stdout);

        // Convert line-by-line reading into a Stream of StreamEvents.
        //
        // Each line is a JSON object.  We parse it and map to our
        // StreamEvent types.  The async_stream macro handles the
        // async iteration naturally.
        let event_stream = async_stream::stream! {
            // We need to keep the child process alive for the duration
            // of the stream.  Moving it into the stream closure ensures
            // it's not dropped (and killed) prematurely.
            let _child = child;

            // Use the BufReader directly with next_line() instead of
            // LinesStream, which avoids type inference issues inside
            // the async_stream macro.
            let mut reader = reader;

            // Track whether we've emitted a MessageComplete event.
            // Claude Code sends a "result" event at the end, which
            // is our signal to emit MessageComplete.
            let mut completed = false;

            // Track whether we received any stream_event text deltas.
            // If we did, we skip "assistant" message text to avoid
            // duplicates.  If we didn't (some Claude Code versions/modes
            // don't emit deltas for every turn), we use assistant messages
            // as fallback.
            let mut got_stream_deltas = false;

            // Parser state for accumulating tool_use blocks from
            // stream_event content_block deltas (same as Anthropic SSE).
            let mut tool_buffers: HashMap<usize, ToolUseBuffer> = HashMap::new();

            // Track thinking block indices so their text_delta events
            // are emitted as ThinkingDelta instead of TextDelta.
            let mut thinking_blocks: std::collections::HashSet<usize> = std::collections::HashSet::new();

            // Read lines from the subprocess stdout.
            //
            // `read_line()` appends to a buffer and returns the number
            // of bytes read.  0 bytes means EOF (process exited).
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
                    break; // EOF — process exited
                }
                let line = line.trim_end().to_string();

                if line.is_empty() {
                    continue;
                }

                // Parse the JSON line.
                let json: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            line = line,
                            error = %e,
                            "failed to parse claude CLI JSON line"
                        );
                        continue;
                    }
                };

                // Dispatch based on the top-level "type" field.
                let event_type = json["type"].as_str().unwrap_or("");

                match event_type {
                    // ---------------------------------------------------
                    // stream_event — raw Anthropic streaming events
                    // wrapped in {"type":"stream_event","event":{...}}.
                    // Same events our Anthropic SSE parser handles.
                    // ---------------------------------------------------
                    "stream_event" => {
                        let inner = &json["event"];
                        let inner_type = inner["type"].as_str().unwrap_or("");

                        match inner_type {
                            "content_block_delta" => {
                                let delta = &inner["delta"];
                                match delta["type"].as_str().unwrap_or("") {
                                    "thinking_delta" => {
                                        if let Some(text) = delta["thinking"].as_str() {
                                            yield Ok(StreamEvent::ThinkingDelta(text.to_string()));
                                        }
                                    }
                                    "text_delta" => {
                                        if let Some(text) = delta["text"].as_str() {
                                            // Route text from thinking blocks as ThinkingDelta.
                                            let idx = inner["index"].as_u64().unwrap_or(0) as usize;
                                            if thinking_blocks.contains(&idx) {
                                                yield Ok(StreamEvent::ThinkingDelta(text.to_string()));
                                            } else {
                                                got_stream_deltas = true;
                                                yield Ok(StreamEvent::TextDelta(text.to_string()));
                                            }
                                        }
                                    }
                                    "input_json_delta" => {
                                        if let Some(partial) = delta["partial_json"].as_str() {
                                            let idx = inner["index"].as_u64().unwrap_or(0) as usize;
                                            if let Some(buf) = tool_buffers.get_mut(&idx) {
                                                buf.json.push_str(partial);
                                            }
                                            yield Ok(StreamEvent::ToolUseInputDelta(partial.to_string()));
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
                                    tool_buffers.insert(idx, ToolUseBuffer {
                                        id: id.clone(), name: name.clone(), json: String::new(),
                                    });
                                    yield Ok(StreamEvent::ToolUseStart { id, name });
                                } else if block_type == "thinking" {
                                    thinking_blocks.insert(idx);
                                }
                            }

                            "content_block_stop" => {
                                let idx = inner["index"].as_u64().unwrap_or(0) as usize;
                                if let Some(buf) = tool_buffers.remove(&idx) {
                                    let input = serde_json::from_str(&buf.json)
                                        .unwrap_or(serde_json::json!({}));
                                    yield Ok(StreamEvent::ToolUseComplete {
                                        id: buf.id, name: buf.name, input,
                                    });
                                }
                            }

                            // message_delta stop reason handled via "result" event.
                            _ => {}
                        }
                    }

                    // ---------------------------------------------------
                    // result — final result with stop_reason and cost.
                    // ---------------------------------------------------
                    "result" => {
                        if !completed {
                            completed = true;
                            let stop_reason = match json["stop_reason"].as_str() {
                                Some("end_turn") => StopReason::EndTurn,
                                Some("tool_use") => StopReason::ToolUse,
                                Some("max_tokens") => StopReason::MaxTokens,
                                _ => StopReason::EndTurn,
                            };
                            if let Some(cost) = json["total_cost_usd"].as_f64() {
                                tracing::info!(
                                    cost_usd = cost,
                                    duration_ms = json["duration_ms"].as_u64().unwrap_or(0),
                                    "claude CLI turn complete"
                                );
                            }
                            yield Ok(StreamEvent::MessageComplete { stop_reason });
                        }
                    }

                    // ---------------------------------------------------
                    // system — session init metadata (model, version).
                    // ---------------------------------------------------
                    "system" => {
                        if let Some(model) = json["model"].as_str() {
                            tracing::debug!(
                                model = model,
                                version = json["claude_code_version"].as_str().unwrap_or(""),
                                "claude CLI session initialized"
                            );
                        }
                    }

                    // ---------------------------------------------------
                    // rate_limit_event — log if throttled.
                    // ---------------------------------------------------
                    "rate_limit_event" => {
                        if json["rate_limit_info"]["status"].as_str() != Some("allowed") {
                            tracing::warn!("claude CLI rate limited");
                        }
                    }

                    // ---------------------------------------------------
                    // assistant — complete assistant message.
                    //
                    // When --include-partial-messages is active, text usually
                    // arrives via stream_event deltas first.  But for multi-
                    // turn tool use, Claude Code may emit assistant messages
                    // without prior deltas (especially for intermediate turns
                    // and the final response after tool calls).
                    //
                    // We extract text from these as a fallback to ensure
                    // nothing is lost.  If stream_event deltas already
                    // delivered the text, we'll get duplicates — but that's
                    // better than empty responses.
                    // ---------------------------------------------------
                    "assistant" => {
                        // Only use assistant messages as fallback when we
                        // haven't received stream_event deltas for this turn.
                        if !got_stream_deltas {
                            if let Some(content) = json["message"]["content"].as_array() {
                                for block in content {
                                    if block["type"].as_str() == Some("text") {
                                        if let Some(text) = block["text"].as_str() {
                                            if !text.is_empty() {
                                                yield Ok(StreamEvent::TextDelta(text.to_string()));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // Reset for the next turn (Claude Code may do
                        // multiple turns with tool calls).
                        got_stream_deltas = false;
                    }

                    // user — tool results from Claude Code's internal loop.
                    "user" => {}

                    other => {
                        tracing::trace!(event_type = other, "unknown claude CLI event type");
                    }
                }
            }

            // If we never got a result event (process killed, crash),
            // emit a MessageComplete so the stream handler doesn't hang.
            if !completed {
                yield Ok(StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                });
            }
        };

        Ok(Box::pin(event_stream))
    }
}

// ---------------------------------------------------------------------------
// ToolUseBuffer — accumulates partial tool_use JSON from stream events.
// ---------------------------------------------------------------------------

/// Same concept as in the Anthropic client — accumulates
/// `input_json_delta` fragments for a tool_use content block.
struct ToolUseBuffer {
    id: String,
    name: String,
    json: String,
}

// ---------------------------------------------------------------------------
// Prompt formatting — convert structured messages into a text prompt.
// ---------------------------------------------------------------------------

/// Format the conversation history and tool definitions into a single
/// text prompt for the `claude -p` command.
///
/// ## Why text formatting instead of structured messages?
///
/// The `claude -p` command takes a single text prompt via stdin.  It
/// doesn't accept structured message arrays like the API does.  So we
/// format the conversation history into a readable text format.
///
/// For single-turn conversations (the common case), the prompt is just
/// the user's latest message.  For multi-turn conversations with tool
/// results, we include the full history so the model has context.
///
/// ## Tool definitions
///
/// When tools are available, we append their definitions to the prompt
/// so the model knows what tools exist.  However, since the underlying
/// API call doesn't include a `tools` parameter, the model cannot emit
/// structured `tool_use` blocks.  This is a known limitation of the
/// Claude Code backend.
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
fn format_prompt(messages: &[Message], tools: &[ToolDefinition]) -> String {
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
    //
    // This is a best-effort approach — the model will see the tools
    // described in text but can't emit structured tool_use blocks.
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
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    

    // -----------------------------------------------------------------------
    // Prompt formatting tests
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
}
