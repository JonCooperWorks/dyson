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
//   serve.rs      — HTTP MCP server (Dyson serves tools TO Claude Code)
//
// Two directions of MCP:
//
//   1. Dyson as MCP CLIENT (this file + transport.rs):
//      Dyson connects to external MCP servers (GitHub, filesystem, etc.),
//      discovers their tools, and wraps them as `Arc<dyn Tool>` for the
//      agent loop.  Configured via `mcp_servers` in dyson.json.
//
//   2. Dyson as MCP SERVER (serve.rs):
//      When using Claude Code as the LLM backend, Dyson starts an HTTP
//      MCP server that exposes workspace tools (view, search, update)
//      to Claude Code.  This lets Claude Code's agent loop access the
//      workspace without Dyson needing to intercept tool calls.
//      See serve.rs for the full architecture documentation.
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
//
// OAuth flow:
//   When an HTTP MCP server has `auth: { "type": "oauth", ... }` in config,
//   on_load() runs the full OAuth 2.0 Authorization Code + PKCE flow:
//
//   1. Check ~/.dyson/tokens/<server>.json for persisted tokens
//   2. If valid → create OAuthAuth, proceed normally
//   3. If no tokens → discover metadata, optionally DCR, generate PKCE,
//      start callback server, log the auth URL, block until callback or
//      timeout (120s), exchange code, persist tokens, proceed with handshake
//
//   This is controller-agnostic: the auth URL is logged via tracing::warn!
//   which the terminal controller displays directly.  For Telegram, the
//   user runs the first auth from a terminal session; subsequent runs use
//   persisted tokens with no interaction needed.
// ===========================================================================

pub mod protocol;
pub mod serve;
pub mod transport;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::auth::oauth;
use crate::auth::oauth_callback;
use crate::auth::oauth_credential::{self, OAuthAuth};
use crate::config::{McpAuthConfig, McpConfig};
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

    /// Run the full OAuth flow: discover → DCR → PKCE → callback → exchange.
    ///
    /// Blocks until the user completes authorization or the timeout expires.
    /// Returns an `OAuthAuth` ready for use with `HttpTransport`.
    ///
    /// The auth URL is logged via `tracing::warn!` so it appears in the
    /// terminal.  This is controller-agnostic — the user clicks the URL
    /// from whatever environment they're in.
    async fn run_oauth_flow(
        server_name: &str,
        url: &str,
        config: &McpAuthConfig,
    ) -> Result<Box<dyn crate::auth::Auth>> {
        let http_client = reqwest::Client::new();

        // 1. Discover metadata (or use overrides from config).
        let metadata = if config.authorization_url.is_some() && config.token_url.is_some() {
            oauth::AuthMetadata {
                authorization_endpoint: config.authorization_url.clone().unwrap(),
                token_endpoint: config.token_url.clone().unwrap(),
                registration_endpoint: config.registration_url.clone(),
                response_types_supported: vec!["code".into()],
                code_challenge_methods_supported: vec!["S256".into()],
                scopes_supported: vec![],
            }
        } else {
            oauth::discover_metadata(url, &http_client).await?
        };

        // 2. Get or register a client ID.
        let (client_id, client_secret) = if let Some(ref cid) = config.client_id {
            (cid.clone(), config.client_secret.clone())
        } else {
            let reg_url = config
                .registration_url
                .as_deref()
                .or(metadata.registration_endpoint.as_deref())
                .ok_or_else(|| {
                    DysonError::oauth(
                        server_name,
                        "no client_id in config and server has no registration endpoint — \
                         set client_id in the auth config",
                    )
                })?;

            let dcr_request = oauth::DcrRequest {
                client_name: format!("dyson-{server_name}"),
                redirect_uris: vec![],
                grant_types: vec!["authorization_code".into(), "refresh_token".into()],
                response_types: vec!["code".into()],
                token_endpoint_auth_method: Some("none".into()),
            };

            let dcr = oauth::register_client(reg_url, &dcr_request, &http_client).await?;
            (dcr.client_id, dcr.client_secret)
        };

        // 3. Generate PKCE pair + state.
        let pkce = oauth::generate_pkce();
        let state = oauth::generate_state();

        // 4. Start callback server.
        let (port, callback_handle, callback_rx) =
            oauth_callback::start_callback_server(&state, Duration::from_secs(300)).await?;

        let redirect_uri = config
            .redirect_uri
            .clone()
            .unwrap_or_else(|| format!("http://127.0.0.1:{port}/callback"));

        // 5. Build auth URL and show it to the user.
        let auth_url = oauth::build_auth_url(
            &metadata,
            &client_id,
            &config.scopes,
            &redirect_uri,
            &pkce.challenge,
            &state,
        );

        tracing::warn!(
            server = server_name,
            "\n\n  OAuth authorization required.\n  Open this URL in your browser:\n\n  {}\n",
            auth_url,
        );

        // 6. Block until the callback fires or timeout (120s).
        //
        // The callback server has a 300s timeout, but we use a shorter one
        // here so on_load() doesn't block forever.  If the user needs more
        // time, they can restart and the flow runs again.
        let callback_result = tokio::time::timeout(Duration::from_secs(120), callback_rx)
            .await
            .map_err(|_| {
                callback_handle.abort();
                DysonError::oauth(
                    server_name,
                    "OAuth authorization timed out after 120 seconds — \
                     restart to try again",
                )
            })?
            .map_err(|_| {
                callback_handle.abort();
                DysonError::oauth(server_name, "OAuth callback server shut down unexpectedly")
            })?;

        callback_handle.abort();

        // 7. Exchange code for tokens.
        let token_response = oauth::exchange_code(
            &metadata.token_endpoint,
            &callback_result.code,
            &pkce.verifier,
            &client_id,
            client_secret.as_deref(),
            &redirect_uri,
            &http_client,
        )
        .await?;

        // 8. Build OAuthAuth and persist tokens.
        let oauth_auth = OAuthAuth::from_token_response(
            &token_response,
            metadata.token_endpoint.clone(),
            client_id,
            client_secret,
        );

        {
            let guard = oauth_auth.credential().read().await;
            oauth_credential::persist_tokens(server_name, &guard).await?;
        }

        tracing::info!(server = server_name, "OAuth authorization complete — tokens persisted");

        Ok(Box::new(oauth_auth))
    }

    /// Create the appropriate Auth impl for an OAuth-configured HTTP transport.
    ///
    /// Tries persisted tokens first.  If none exist (or expired without a
    /// refresh token), runs the interactive OAuth flow (blocks until complete).
    async fn create_oauth_auth(
        server_name: &str,
        url: &str,
        config: &McpAuthConfig,
    ) -> Result<Box<dyn crate::auth::Auth>> {
        // Try loading persisted tokens.
        if let Some(credential) = oauth_credential::load_tokens(server_name).await? {
            if credential.refresh_token.is_some() {
                tracing::info!(
                    server = server_name,
                    expired = credential.is_expired(),
                    "using persisted OAuth tokens"
                );
                return Ok(Box::new(OAuthAuth::new(credential)));
            }

            if !credential.is_expired() {
                tracing::info!(server = server_name, "using persisted OAuth access token");
                return Ok(Box::new(OAuthAuth::new(credential)));
            }

            tracing::warn!(
                server = server_name,
                "persisted tokens expired with no refresh token — starting new OAuth flow"
            );
        }

        Self::run_oauth_flow(server_name, url, config).await
    }

    /// Perform the MCP initialization handshake and discover tools.
    async fn do_mcp_handshake(
        &mut self,
        server_name: &str,
        transport: &Arc<dyn McpTransport>,
    ) -> Result<()> {
        let init_params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "dyson",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let init_result = transport
            .send_request("initialize", Some(init_params))
            .await?;

        tracing::debug!(
            server = server_name,
            result = %init_result,
            "MCP initialize response"
        );

        transport
            .send_notification("notifications/initialized", None)
            .await?;

        let tools_result = transport
            .send_request("tools/list", Some(serde_json::json!({})))
            .await?;

        let tool_defs: Vec<McpToolDef> = match tools_result.get("tools") {
            Some(tools_json) => {
                serde_json::from_value(tools_json.clone()).map_err(|e| DysonError::Mcp {
                    server: server_name.to_string(),
                    message: format!("failed to parse tools/list: {e}"),
                })?
            }
            None => vec![],
        };

        tracing::info!(
            server = server_name,
            tool_count = tool_defs.len(),
            "MCP tools discovered"
        );

        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut tool_descs: Vec<String> = Vec::new();

        for def in tool_defs {
            let desc = def.description.clone().unwrap_or_default();
            tool_descs.push(format!("- **{}**: {}", def.name, desc));

            tools.push(Arc::new(McpRemoteTool {
                tool_name: def.name,
                description: desc,
                input_schema: def
                    .input_schema
                    .unwrap_or(serde_json::json!({"type": "object"})),
                transport: Arc::clone(transport),
                server_name: server_name.to_string(),
            }));
        }

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
    /// For stdio transports: spawns the process and runs the MCP handshake.
    /// For HTTP transports without OAuth: uses static headers.
    /// For HTTP transports with OAuth: loads persisted tokens or blocks on
    /// the interactive OAuth flow, then runs the MCP handshake.
    async fn on_load(&mut self) -> Result<()> {
        let server_name = self.config.name.clone();

        tracing::info!(server = %server_name, "connecting to MCP server");

        // Clone transport config to avoid borrow conflicts with &mut self.
        let transport: Arc<dyn McpTransport> = match self.config.transport.clone() {
            crate::config::McpTransportConfig::Stdio { command, args, env } => {
                Arc::new(StdioTransport::spawn(&command, &args, &env).await?)
            }
            crate::config::McpTransportConfig::Http {
                url,
                headers,
                auth: None,
            } => {
                let auth = Box::new(crate::auth::StaticHeadersAuth::new(headers));
                Arc::new(HttpTransport::new(&url, auth))
            }
            crate::config::McpTransportConfig::Http {
                url,
                auth: Some(oauth_config),
                ..
            } => {
                // OAuth: load persisted tokens or run interactive flow (blocks).
                let auth = Self::create_oauth_auth(&server_name, &url, &oauth_config).await?;
                Arc::new(HttpTransport::new(&url, auth))
            }
        };

        self.do_mcp_handshake(&server_name, &transport).await?;
        self.transport = Some(transport);
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
    async fn run(&self, input: &serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
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

        let tool_result: McpToolResult =
            serde_json::from_value(result_json).map_err(|e| DysonError::Mcp {
                server: self.server_name.clone(),
                message: format!("failed to parse tools/call result: {e}"),
            })?;

        let content: String = tool_result
            .content
            .iter()
            .filter_map(|c| match c {
                McpContent::Text { text } => Some(text.as_str()),
                McpContent::Unknown => None,
            })
            .fold(String::new(), |mut acc, s| {
                if !acc.is_empty() {
                    acc.push('\n');
                }
                acc.push_str(s);
                acc
            });

        Ok(ToolOutput {
            content,
            is_error: tool_result.is_error,
            metadata: None,
            files: Vec::new(),
        })
    }
}
