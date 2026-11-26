//! Shared helpers for CLI-subprocess providers (Claude Code, Codex):
//! the per-call MCP server, binary resolution, and prompt flattening.

use super::*;

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

/// Start an MCP server exposing workspace + Dyson tools to CLI subprocesses.
/// Fresh server per `stream()` call; lives until the returned handle is dropped.
pub(crate) async fn start_mcp_server(
    workspace: &crate::workspace::WorkspaceHandle,
    extra_tools: std::collections::HashMap<String, std::sync::Arc<dyn Tool>>,
) -> Result<McpServerInfo> {
    use crate::skill::mcp::serve::McpHttpServer;
    use std::sync::Arc;

    let server = Arc::new(McpHttpServer::new(Arc::clone(workspace), extra_tools));

    let (port, handle, token) = server.start().await.map_err(|e| {
        dyson_core::error::DysonError::Llm(format!("failed to start MCP HTTP server: {e}"))
    })?;

    let url = format!("http://127.0.0.1:{port}/mcp");

    Ok(McpServerInfo {
        port,
        handle,
        token,
        url,
    })
}

/// Start an MCP server for forwarded Dyson tools when a CLI client has no
/// workspace handle. This is the normal HTTP/Swarm controller construction
/// path: external tools must remain available, while the workspace capability
/// must not be backed by an unrelated agent's state.
pub(crate) async fn start_mcp_tools_server(
    extra_tools: std::collections::HashMap<String, std::sync::Arc<dyn Tool>>,
) -> Result<McpServerInfo> {
    use crate::skill::mcp::serve::McpHttpServer;
    use std::sync::Arc;

    let server = Arc::new(McpHttpServer::new_tools_only(extra_tools));
    let (port, handle, token) = server.start().await.map_err(|e| {
        dyson_core::error::DysonError::Llm(format!("failed to start MCP HTTP server: {e}"))
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
                    ContentBlock::Document { extracted_text, .. } => {
                        let _ = write!(prompt, "[PDF document]\n{extracted_text}\n\n");
                    }
                    ContentBlock::Thinking { .. } => {}
                    ContentBlock::Artefact { .. } => {}
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
