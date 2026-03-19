// ===========================================================================
// MCP skill — connect to MCP servers and expose their tools to the agent.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements the `Skill` trait for MCP (Model Context Protocol) servers.
//   An McpSkill connects to an MCP server, discovers its tools via
//   `tools/list`, and wraps each as a `Tool` impl.  The agent loop never
//   knows MCP exists — it just sees tools.
//
// Module layout:
//   mod.rs        — McpSkill (Skill impl) + McpRemoteTool (Tool impl)
//   protocol.rs   — JSON-RPC message types (requests, responses, tool defs)
//   transport.rs  — Stdio transport (spawn process, read/write JSON-RPC)
//
// How it works:
//
//   dyson.json:
//     "mcp_servers": {
//       "github": { "command": "npx", "args": [...], "env": {...} }
//     }
//
//   McpSkill::on_load():
//     1. Spawn the process via StdioTransport
//     2. Send "initialize" → get server capabilities
//     3. Send "initialized" notification
//     4. Send "tools/list" → get tool definitions
//     5. Wrap each tool as McpRemoteTool (implements Tool trait)
//
//   Agent uses the tools:
//     agent.run("search github for X")
//       → LLM calls tool "github_search_repos"
//       → agent looks up tool in HashMap
//       → McpRemoteTool.run(input)
//         → StdioTransport.send_request("tools/call", {name, arguments})
//         → MCP server executes, returns result
//         → McpRemoteTool returns ToolOutput
//
// Why MCP is "not special":
//   The agent loop calls tool.run() — it doesn't know whether that tool
//   is a BashTool (local), McpRemoteTool (remote), or anything else.
//   MCP is just another Skill implementation.  No special-casing anywhere.
// ===========================================================================

pub mod protocol;
pub mod serve;
pub mod transport;

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::McpConfig;
use crate::error::{DysonError, Result};
use crate::skill::Skill;
use crate::tool::{Tool, ToolContext, ToolOutput};

use self::protocol::{McpContent, McpToolDef, McpToolResult};
use self::transport::{HttpTransport, McpTransport, StdioTransport};

// ---------------------------------------------------------------------------
// McpSkill — a Skill backed by an MCP server connection.
// ---------------------------------------------------------------------------

/// Connects to an MCP server and exposes its tools to the agent.
///
/// Created from an `McpConfig` (parsed from dyson.json).  The actual
/// connection and tool discovery happen in `on_load()`.  Before that,
/// `tools()` returns an empty slice.
pub struct McpSkill {
    /// Config from dyson.json (name, transport details).
    config: McpConfig,

    /// The transport to the MCP server (stdio or HTTP).
    ///
    /// `None` until `on_load()` is called.  Shared (via Arc) with all
    /// `McpRemoteTool` instances so they can send `tools/call` requests.
    transport: Option<Arc<dyn McpTransport>>,

    /// Tools discovered from the server via `tools/list`.
    ///
    /// Populated during `on_load()`.  Each is an `McpRemoteTool` that
    /// forwards calls to the MCP server.
    tools: Vec<Arc<dyn Tool>>,

    /// System prompt fragment listing the available tools.
    system_prompt: Option<String>,
}

impl McpSkill {
    /// Create a new McpSkill from config.  Does NOT connect yet —
    /// call `on_load()` to establish the connection.
    pub fn new(config: McpConfig) -> Self {
        Self {
            config,
            transport: None,
            tools: Vec::new(),
            system_prompt: None,
        }
    }
}

#[async_trait]
impl Skill for McpSkill {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    /// Connect to the MCP server and discover its tools.
    ///
    /// ## MCP handshake sequence
    ///
    /// 1. Spawn the server process (stdio transport)
    /// 2. Send `initialize` with our client info
    /// 3. Send `initialized` notification (no response expected)
    /// 4. Send `tools/list` to discover available tools
    /// 5. Wrap each tool as an `McpRemoteTool`
    async fn on_load(&mut self) -> Result<()> {
        let server_name = &self.config.name;

        tracing::info!(server = server_name, "connecting to MCP server");

        // -- Create the transport --
        let transport: Arc<dyn McpTransport> = match &self.config.transport {
            crate::config::McpTransportConfig::Stdio { command, args, env } => {
                Arc::new(StdioTransport::spawn(command, args, env).await?)
            }
            crate::config::McpTransportConfig::Http { url, headers } => {
                Arc::new(HttpTransport::new(url, headers.clone()))
            }
        };

        // -- Initialize handshake --
        let init_params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "dyson",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let init_result = transport.send_request("initialize", Some(init_params)).await?;

        tracing::debug!(
            server = server_name,
            result = %init_result,
            "MCP initialize response"
        );

        // Send initialized notification.
        transport
            .send_notification("notifications/initialized", None)
            .await?;

        // -- Discover tools --
        let tools_result = transport.send_request("tools/list", Some(serde_json::json!({}))).await?;

        let tool_defs: Vec<McpToolDef> = match tools_result.get("tools") {
            Some(tools_json) => serde_json::from_value(tools_json.clone()).map_err(|e| {
                DysonError::Mcp {
                    server: server_name.clone(),
                    message: format!("failed to parse tools/list: {e}"),
                }
            })?,
            None => vec![],
        };

        tracing::info!(
            server = server_name,
            tool_count = tool_defs.len(),
            "MCP tools discovered"
        );

        // -- Wrap each tool --
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut tool_descs: Vec<String> = Vec::new();

        for def in tool_defs {
            let desc = def.description.clone().unwrap_or_default();
            tool_descs.push(format!("- **{}**: {}", def.name, desc));

            tools.push(Arc::new(McpRemoteTool {
                tool_name: def.name,
                description: desc,
                input_schema: def.input_schema.unwrap_or(serde_json::json!({"type": "object"})),
                transport: Arc::clone(&transport),
                server_name: server_name.clone(),
            }));
        }

        self.transport = Some(transport);
        self.tools = tools;

        if !tool_descs.is_empty() {
            self.system_prompt = Some(format!(
                "MCP server '{}' provides these tools:\n{}",
                server_name,
                tool_descs.join("\n")
            ));
        }

        Ok(())
    }

    async fn on_unload(&mut self) -> Result<()> {
        // Drop the transport — this drops the Arc, and when the last
        // McpRemoteTool is also dropped, the child process's stdin
        // closes, causing it to exit.
        self.transport = None;
        self.tools.clear();
        tracing::info!(server = self.config.name, "MCP server disconnected");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// McpRemoteTool — a Tool backed by a tools/call RPC to an MCP server.
// ---------------------------------------------------------------------------

/// A single tool from an MCP server, discovered via `tools/list`.
///
/// When the agent calls `run()`, this sends a `tools/call` JSON-RPC
/// request to the MCP server and returns the result.  The agent doesn't
/// know this tool is remote — it's just another `Arc<dyn Tool>`.
struct McpRemoteTool {
    /// Tool name as reported by the MCP server.
    tool_name: String,

    /// Tool description.
    description: String,

    /// JSON Schema for the tool's input.
    input_schema: serde_json::Value,

    /// Shared transport to the MCP server (stdio or HTTP).
    transport: Arc<dyn McpTransport>,

    /// Server name (for error messages).
    server_name: String,
}

#[async_trait]
impl Tool for McpRemoteTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    /// Execute the tool by sending `tools/call` to the MCP server.
    ///
    /// The MCP tools/call request looks like:
    /// ```json
    /// {
    ///   "method": "tools/call",
    ///   "params": {
    ///     "name": "search_repos",
    ///     "arguments": { "query": "rust agent" }
    ///   }
    /// }
    /// ```
    ///
    /// The response contains `content` (array of text/image blocks) and
    /// an `isError` flag.
    async fn run(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        tracing::debug!(
            server = self.server_name,
            tool = self.tool_name,
            "calling MCP tool"
        );

        let params = serde_json::json!({
            "name": self.tool_name,
            "arguments": input
        });

        let result_json = self
            .transport
            .send_request("tools/call", Some(params))
            .await
            .map_err(|e| DysonError::Mcp {
                server: self.server_name.clone(),
                message: format!("tools/call failed for '{}': {e}", self.tool_name),
            })?;

        // Parse the MCP tool result.
        let tool_result: McpToolResult =
            serde_json::from_value(result_json).map_err(|e| DysonError::Mcp {
                server: self.server_name.clone(),
                message: format!("failed to parse tools/call result: {e}"),
            })?;

        // Concatenate text content blocks into a single string.
        let content: String = tool_result
            .content
            .iter()
            .filter_map(|c| match c {
                McpContent::Text { text } => Some(text.as_str()),
                McpContent::Unknown => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolOutput {
            content,
            is_error: tool_result.is_error,
            metadata: None,
        })
    }
}
