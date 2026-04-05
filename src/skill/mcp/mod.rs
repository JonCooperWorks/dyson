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
// OAuth flow (non-blocking):
//   When an HTTP MCP server has `auth: { "type": "oauth", ... }` in config,
//   on_load() checks for persisted tokens:
//
//   1. Tokens exist → create OAuthAuth, proceed with MCP handshake immediately
//   2. No tokens → start callback server as background task, return Ok(())
//      with zero tools and a system prompt containing the auth URL.
//      The background task waits for the callback, exchanges the code,
//      persists tokens, and shuts down.  The next hot reload or restart
//      picks up the persisted tokens and loads tools normally.
//
//   This never blocks the agent.  The user authorizes on their own time,
//   and tools appear after the next config reload or restart.
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

    /// Start a background task that runs the OAuth flow and persists tokens.
    ///
    /// Returns the auth URL for the system prompt.  The background task:
    /// 1. Waits for the OAuth callback (up to 5 minutes)
    /// 2. Exchanges the authorization code for tokens
    /// 3. Persists tokens to ~/.dyson/tokens/<server>.json
    /// 4. Logs success
    ///
    /// The skill loads with zero tools.  After the user authorizes and the
    /// background task persists tokens, the next hot reload or restart will
    /// pick up the tokens and load tools normally.
    async fn start_oauth_background(
        server_name: &str,
        url: &str,
        config: &McpAuthConfig,
    ) -> Result<String> {
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

        // 5. Build auth URL.
        let auth_url = oauth::build_auth_url(
            &metadata,
            &client_id,
            &config.scopes,
            &redirect_uri,
            &pkce.challenge,
            &state,
        );

        // 6. Spawn background task to wait for callback and persist tokens.
        //
        // This runs independently of the agent.  When the user clicks the
        // auth URL and authorizes, the callback fires, the task exchanges
        // the code for tokens, persists them, and exits.  The agent doesn't
        // need to know — the next on_load() will find the persisted tokens.
        let server_name_owned = server_name.to_string();
        let token_endpoint = metadata.token_endpoint.clone();
        tokio::spawn(async move {
            let result = async {
                // Wait for the callback (up to 5 minutes).
                let callback_result = callback_rx.await.map_err(|_| {
                    DysonError::oauth(&server_name_owned, "callback channel closed")
                })?;

                // Exchange code for tokens.
                let http_client = reqwest::Client::new();
                let token_response = oauth::exchange_code(
                    &token_endpoint,
                    &callback_result.code,
                    &pkce.verifier,
                    &client_id,
                    client_secret.as_deref(),
                    &redirect_uri,
                    &http_client,
                )
                .await?;

                // Persist tokens directly — no need to construct OAuthAuth.
                oauth_credential::persist_token_response(
                    &server_name_owned,
                    &token_response,
                    &token_endpoint,
                    &client_id,
                    client_secret.as_deref(),
                )
                .await?;

                tracing::info!(
                    server = %server_name_owned,
                    "OAuth tokens persisted — reload config to connect"
                );

                Ok::<(), DysonError>(())
            }
            .await;

            callback_handle.abort();

            if let Err(e) = result {
                tracing::warn!(
                    server = %server_name_owned,
                    error = %e,
                    "OAuth background flow failed"
                );
            }
        });

        Ok(auth_url)
    }

    /// Create the appropriate Auth impl for an OAuth-configured HTTP transport.
    ///
    /// Returns `Ok(Some(auth))` if persisted tokens were found.
    /// Returns `Ok(None)` if no tokens exist (caller should start background flow).
    async fn load_oauth_auth(
        server_name: &str,
    ) -> Result<Option<Box<dyn crate::auth::Auth>>> {
        if let Some(credential) = oauth_credential::load_tokens(server_name).await? {
            if credential.refresh_token.is_some() {
                tracing::info!(
                    server = server_name,
                    expired = credential.is_expired(),
                    "using persisted OAuth tokens"
                );
                return Ok(Some(Box::new(OAuthAuth::new(credential))));
            }

            if !credential.is_expired() {
                tracing::info!(server = server_name, "using persisted OAuth access token");
                return Ok(Some(Box::new(OAuthAuth::new(credential))));
            }

            tracing::warn!(
                server = server_name,
                "persisted tokens expired with no refresh token"
            );
        }

        Ok(None)
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
    /// For HTTP transports with OAuth:
    ///   - If persisted tokens exist → loads them, runs handshake, tools available
    ///   - If no tokens → starts background OAuth flow, returns immediately
    ///     with zero tools and a system prompt containing the auth URL.
    ///     The agent tells the user to authorize.  After authorization, the
    ///     background task persists tokens.  Next hot reload loads them.
    ///
    /// Never blocks the agent.
    async fn on_load(&mut self) -> Result<()> {
        let server_name = self.config.name.clone();

        tracing::info!(server = %server_name, "connecting to MCP server");

        // Clone transport config to avoid borrow conflicts with &mut self.
        let transport: Option<Arc<dyn McpTransport>> = match self.config.transport.clone() {
            crate::config::McpTransportConfig::Stdio { command, args, env } => {
                Some(Arc::new(StdioTransport::spawn(&command, &args, &env).await?))
            }
            crate::config::McpTransportConfig::Http {
                url,
                headers,
                auth: None,
            } => {
                let auth = Box::new(crate::auth::StaticHeadersAuth::new(headers));
                Some(Arc::new(HttpTransport::new(&url, auth)))
            }
            crate::config::McpTransportConfig::Http {
                url,
                auth: Some(oauth_config),
                ..
            } => {
                // Try persisted tokens first.
                match Self::load_oauth_auth(&server_name).await? {
                    Some(auth) => Some(Arc::new(HttpTransport::new(&url, auth))),
                    None => {
                        // No tokens — start background OAuth flow.
                        // The skill loads with zero tools; the agent will
                        // tell the user to authorize via the system prompt.
                        let auth_url = Self::start_oauth_background(
                            &server_name,
                            &url,
                            &oauth_config,
                        )
                        .await?;

                        self.system_prompt = Some(format!(
                            "**MCP server '{server_name}' requires OAuth authorization.**\n\n\
                             The user must open this URL in their browser to authorize:\n\
                             {auth_url}\n\n\
                             After authorizing, ask the user to reload the configuration \
                             so the server's tools become available.",
                        ));

                        tracing::warn!(
                            server = %server_name,
                            "OAuth authorization required — see system prompt for URL"
                        );

                        // Return Ok with zero tools — don't block, don't error.
                        return Ok(());
                    }
                }
            }
        };

        if let Some(ref t) = transport {
            self.do_mcp_handshake(&server_name, t).await?;
        }
        self.transport = transport;
        Ok(())
    }

    async fn on_unload(&mut self) -> Result<()> {
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
