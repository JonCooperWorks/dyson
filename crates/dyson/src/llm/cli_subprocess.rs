// ===========================================================================
// CLI subprocess infrastructure — shared code for CLI-based LLM clients.
//
// Both `ClaudeCodeClient` and `CodexClient` spawn a CLI subprocess for each
// LLM turn, read JSONL events from stdout, and parse them into `StreamEvent`s.
// This module extracts the shared pieces:
//
//   - `CliLineParser` trait: parse one JSONL line → Vec<Result<StreamEvent>>
//   - `cli_event_stream()`: generic async stream from a subprocess stdout
//   - Process spawning helpers
//
// Each client still owns its `StreamParserState` (the parsing logic differs
// significantly between Claude Code and Codex), but the streaming boilerplate
// is shared.
// ===========================================================================

use std::collections::HashMap;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStdout;

use crate::error::{DysonError, Result};
use crate::llm::stream::StreamEvent;
use crate::llm::{StreamResponse, ToolDefinition, ToolMode};

/// Trait for JSONL line parsers used by CLI subprocess clients.
///
/// Each CLI client implements this for its specific event format.
/// The shared `cli_event_stream()` function calls `parse_line()` for
/// each line and `finalize()` at EOF.
pub trait CliLineParser: Send + 'static {
    /// Parse one JSONL line. Returns events to yield (may be empty).
    fn parse_line(&mut self, line: &str) -> Vec<Result<StreamEvent>>;

    /// Called after EOF. Returns any final events (e.g. error if no
    /// completion event was received).
    fn finalize(&mut self) -> Vec<Result<StreamEvent>>;
}

/// Create a stream of `StreamEvent`s by reading JSONL lines from a
/// child process's stdout.
///
/// This is the shared streaming core for CLI subprocess LLM clients.
/// Each client provides its own `CliLineParser` implementation, but the
/// line reading loop, error handling, and lifetime management are identical.
///
/// ## Ownership
///
/// The `_keep_alive` parameter accepts arbitrary `Send + 'static` values
/// that need to live for the stream's duration (e.g. the child process
/// handle, MCP server task handle).  They're moved into the async closure
/// and dropped when the stream ends.
pub fn cli_event_stream<P: CliLineParser>(
    stdout: ChildStdout,
    parser: P,
    _keep_alive: Vec<Box<dyn std::any::Any + Send>>,
) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<StreamEvent>> + Send>> {
    Box::pin(async_stream::stream! {
        let _owned = _keep_alive;
        let reader = BufReader::new(stdout);
        let mut reader = reader;
        let mut parser = parser;

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
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }

            for event in parser.parse_line(line) {
                yield event;
            }
        }

        for event in parser.finalize() {
            yield event;
        }
    })
}

/// Build a `StreamResponse` for CLI clients that observe tool execution
/// (the subprocess handles tools internally, Dyson doesn't execute them).
pub fn build_observe_response(
    stream: std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<StreamEvent>> + Send>>,
) -> StreamResponse {
    StreamResponse {
        stream,
        tool_mode: ToolMode::Observe,
        input_tokens: None,
        swarm_llm_audit_id: None,
        provider: None,
        model: None,
    }
}

/// Filter tool definitions for CLI clients.
///
/// When a workspace is available, tools are served to the subprocess via
/// MCP — return an empty list so the text prompt doesn't duplicate them.
/// Otherwise, include non-agent-only tools for text-based tool descriptions.
pub fn filter_tools_for_cli(tools: &[ToolDefinition], has_workspace: bool) -> Vec<&ToolDefinition> {
    if has_workspace {
        vec![]
    } else {
        tools.iter().filter(|t| !t.agent_only).collect()
    }
}

pub(crate) fn sanitized_child_env<I>(env: I) -> HashMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    env.into_iter()
        .filter(|(k, _)| !is_secret_env_name(k))
        .collect()
}

fn is_secret_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("SWARM_")
        || upper.starts_with("DYSON_")
        || matches!(
            upper.as_str(),
            "ANTHROPIC_API_KEY"
                | "OPENAI_API_KEY"
                | "OPENROUTER_API_KEY"
                | "GEMINI_API_KEY"
                | "GOOGLE_API_KEY"
                | "OLLAMA_API_KEY"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn sanitized_child_env_removes_swarm_and_provider_secrets() {
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/usr/bin".to_string());
        env.insert("HOME".to_string(), "/home/dyson".to_string());
        env.insert("SWARM_PROXY_TOKEN".to_string(), "pt_secret".to_string());
        env.insert("SWARM_INGEST_TOKEN".to_string(), "it_secret".to_string());
        env.insert("ANTHROPIC_API_KEY".to_string(), "sk-ant-secret".to_string());
        env.insert("OPENAI_API_KEY".to_string(), "sk-openai-secret".to_string());

        let sanitized = sanitized_child_env(env);
        assert_eq!(sanitized.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(
            sanitized.get("HOME").map(String::as_str),
            Some("/home/dyson")
        );
        assert!(!sanitized.contains_key("SWARM_PROXY_TOKEN"));
        assert!(!sanitized.contains_key("SWARM_INGEST_TOKEN"));
        assert!(!sanitized.contains_key("ANTHROPIC_API_KEY"));
        assert!(!sanitized.contains_key("OPENAI_API_KEY"));
    }
}
