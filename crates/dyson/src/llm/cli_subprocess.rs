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
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStdout;

use crate::llm::stream::StreamEvent;
use crate::llm::{StreamResponse, ToolDefinition, ToolMode};
use crate::tool::Tool;
use dyson_core::error::{DysonError, Result};

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
    keep_alive: Vec<Box<dyn std::any::Any + Send>>,
) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<StreamEvent>> + Send>> {
    lines_event_stream(stdout, parser, keep_alive, MAX_CLI_LINE_BYTES)
}

/// Longest single JSONL line accepted from a CLI subprocess.  Lines
/// carry whole tool results, so this is generous; it exists so a
/// runaway child cannot make Dyson buffer an unbounded line.
pub(crate) const MAX_CLI_LINE_BYTES: usize = 64 * 1024 * 1024;

fn lines_event_stream<R, P>(
    reader: R,
    parser: P,
    keep_alive: Vec<Box<dyn std::any::Any + Send>>,
    max_line_bytes: usize,
) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<StreamEvent>> + Send>>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    P: CliLineParser,
{
    Box::pin(async_stream::stream! {
        let _owned = keep_alive;
        let mut reader = BufReader::new(reader);
        let mut parser = parser;
        let mut buf = Vec::new();

        loop {
            buf.clear();
            match read_line_capped(&mut reader, &mut buf, max_line_bytes).await {
                Ok(0) => break, // EOF
                Ok(_) => {}
                Err(e) => {
                    yield Err(DysonError::Io(e));
                    break;
                }
            }
            let line = String::from_utf8_lossy(&buf);
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

/// `read_until(b'\n')` that fails instead of growing `buf` past `max`.
/// Returns the bytes consumed (0 at EOF).
async fn read_line_capped<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<usize>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut consumed_total = 0;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(consumed_total);
        }
        let (take, done) = match available.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        if buf.len().saturating_add(take) > max.saturating_add(1) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("CLI output line exceeds {max} bytes"),
            ));
        }
        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        consumed_total += take;
        if done {
            return Ok(consumed_total);
        }
    }
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
/// When a per-turn MCP server is available, tools are served to the subprocess
/// through it — return an empty list so the text prompt doesn't duplicate them.
/// Otherwise, include non-agent-only tools for text-based tool descriptions.
pub fn filter_tools_for_cli(
    tools: &[ToolDefinition],
    has_mcp_server: bool,
) -> Vec<&ToolDefinition> {
    if has_mcp_server {
        vec![]
    } else {
        tools.iter().filter(|t| !t.agent_only).collect()
    }
}

/// Select the concrete tool implementations advertised for this exact LLM
/// call. The client registry is shared across conversations, so keeping this
/// map on a cached CLI client would let the last-created agent overwrite every
/// other chat's MCP surface.
pub fn forwarded_tools_for_call(
    definitions: &[ToolDefinition],
    instances: &HashMap<String, Arc<dyn Tool>>,
) -> HashMap<String, Arc<dyn Tool>> {
    definitions
        .iter()
        .filter(|definition| !definition.agent_only)
        .filter_map(|definition| {
            let tool = instances.get(&definition.name)?;
            (!tool.agent_only()).then(|| (definition.name.clone(), Arc::clone(tool)))
        })
        .collect()
}

pub(crate) fn sanitized_child_env<I>(env: I) -> HashMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    env.into_iter()
        .filter(|(k, _)| !is_secret_env_name(k))
        .collect()
}

/// Return the managed Swarm subscription proxy and the instance-scoped
/// bearer. The bearer is intentionally the same source-IP-bound `pt_` token
/// used by ordinary Swarm inference; it is not a provider OAuth credential.
pub(crate) fn subscription_proxy(route: &str) -> Option<(String, String)> {
    let (base, token) = crate::swarm_cost::runtime_proxy_parts()?;
    let base = base.trim().trim_end_matches('/');
    // The post-warmup configure patch carries the active OpenAI-compatible
    // provider URL (`.../llm/openrouter`), while native subscription routes
    // are siblings beneath the `/llm` apex. Boot-time SWARM_PROXY_URL uses
    // the apex already, so accept both shapes without ever falling through
    // the OpenRouter funding/key path.
    let base = base.strip_suffix("/openrouter").unwrap_or(base);
    let token = token.trim();
    if base.is_empty() || token.is_empty() {
        return None;
    }
    Some((
        format!("{base}/{}", route.trim_matches('/')),
        token.to_owned(),
    ))
}

fn is_secret_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("SWARM_")
        || upper.starts_with("DYSON_")
        || matches!(
            upper.as_str(),
            "ANTHROPIC_API_KEY"
                | "ANTHROPIC_AUTH_TOKEN"
                | "CLAUDE_CODE_OAUTH_TOKEN"
                | "OPENAI_API_KEY"
                | "OPENROUTER_API_KEY"
                | "GEMINI_API_KEY"
                | "GOOGLE_API_KEY"
                | "OLLAMA_API_KEY"
        )
}

#[cfg(test)]
mod tests {

    struct CountingParser(Arc<std::sync::atomic::AtomicUsize>);

    impl CliLineParser for CountingParser {
        fn parse_line(&mut self, line: &str) -> Vec<Result<StreamEvent>> {
            self.0
                .fetch_add(line.len(), std::sync::atomic::Ordering::SeqCst);
            Vec::new()
        }
        fn finalize(&mut self) -> Vec<Result<StreamEvent>> {
            Vec::new()
        }
    }

    #[tokio::test]
    async fn oversized_cli_line_is_refused_not_buffered() {
        // MEM-09: one newline-free line used to be buffered whole.
        use futures_util::StreamExt as _;
        use tokio::io::AsyncReadExt as _;
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reader = tokio::io::repeat(b'a').take(64 * 1024);
        let mut stream =
            lines_event_stream(reader, CountingParser(Arc::clone(&seen)), Vec::new(), 1024);
        let mut errors = 0;
        while let Some(item) = stream.next().await {
            errors += usize::from(item.is_err());
        }
        assert_eq!(errors, 1, "oversized line must surface as an error");
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 0);

        // Lines within the cap still parse.
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reader = std::io::Cursor::new(b"abc\ndefg\n".to_vec());
        let mut stream =
            lines_event_stream(reader, CountingParser(Arc::clone(&seen)), Vec::new(), 4);
        while let Some(item) = stream.next().await {
            assert!(item.is_ok());
        }
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 7);
    }
    use super::*;
    use std::collections::HashMap;

    struct NamedTool {
        name: &'static str,
        agent_only: bool,
    }

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "test tool"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn agent_only(&self) -> bool {
            self.agent_only
        }

        async fn run(
            &self,
            _input: &serde_json::Value,
            _ctx: &crate::tool::ToolContext,
        ) -> dyson_core::error::Result<crate::tool::ToolOutput> {
            Ok(crate::tool::ToolOutput::success("ok"))
        }
    }

    fn definition(name: &str, agent_only: bool) -> ToolDefinition {
        ToolDefinition {
            name: name.to_owned(),
            description: "test tool".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
            agent_only,
        }
    }

    #[test]
    fn forwarded_tools_are_scoped_to_the_current_call() {
        let mut instances: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        instances.insert(
            "axelrod_tool".into(),
            Arc::new(NamedTool {
                name: "axelrod_tool",
                agent_only: false,
            }),
        );
        instances.insert(
            "other_chat_tool".into(),
            Arc::new(NamedTool {
                name: "other_chat_tool",
                agent_only: false,
            }),
        );
        instances.insert(
            "agent_only".into(),
            Arc::new(NamedTool {
                name: "agent_only",
                agent_only: true,
            }),
        );

        let selected = forwarded_tools_for_call(
            &[
                definition("axelrod_tool", false),
                definition("agent_only", true),
            ],
            &instances,
        );
        assert_eq!(selected.len(), 1);
        assert!(selected.contains_key("axelrod_tool"));
        assert!(forwarded_tools_for_call(&[], &instances).is_empty());
    }

    #[test]
    fn sanitized_child_env_removes_swarm_and_provider_secrets() {
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/usr/bin".to_string());
        env.insert("HOME".to_string(), "/home/dyson".to_string());
        env.insert("SWARM_PROXY_TOKEN".to_string(), "pt_secret".to_string());
        env.insert("SWARM_INGEST_TOKEN".to_string(), "it_secret".to_string());
        env.insert("ANTHROPIC_API_KEY".to_string(), "sk-ant-secret".to_string());
        env.insert(
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            "oauth-secret".to_string(),
        );
        env.insert(
            "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
            "oauth-secret".to_string(),
        );
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
        assert!(!sanitized.contains_key("ANTHROPIC_AUTH_TOKEN"));
        assert!(!sanitized.contains_key("CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(!sanitized.contains_key("OPENAI_API_KEY"));
    }

    #[tokio::test]
    async fn native_subscription_uses_post_warmup_runtime_patch() {
        let _guard = crate::swarm_cost::test_config_guard().await;
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::swarm_cost::set_runtime_config(None);
            }
        }
        let _reset = Reset;
        crate::swarm_cost::set_runtime_config_from_parts(
            "http://169.254.68.5:8080/llm/openrouter/",
            "pt_runtime_only",
        );

        assert_eq!(
            subscription_proxy("codex-subscription"),
            Some((
                "http://169.254.68.5:8080/llm/codex-subscription".into(),
                "pt_runtime_only".into(),
            ))
        );
    }
}
