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
// ===========================================================================

pub mod protocol;
pub mod serve;
pub mod transport;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

use crate::auth::oauth;
use crate::auth::oauth_callback;
use crate::auth::oauth_credential::{self, OAuthAuth, OAuthCredential};
use crate::auth::Credential;
use crate::config::{McpAuthConfig, McpConfig};
use crate::error::{DysonError, Result};
use crate::skill::Skill;
use crate::tool::{Tool, ToolContext, ToolOutput};

use self::protocol::{McpContent, McpToolDef, McpToolResult};
use self::transport::{HttpTransport, McpTransport, StdioTransport};

// ---------------------------------------------------------------------------
// McpSkill — a Skill backed by an MCP server connection.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// OAuthPendingAuth — state for an in-progress OAuth flow.
// ---------------------------------------------------------------------------

/// Holds the state for an OAuth flow that's waiting for the user to
/// authorize in their browser.
///
/// Created during `on_load()` when an OAuth-configured MCP server has no
/// persisted tokens.  The callback server is running and waiting for the
/// redirect.  The auth URL is surfaced to the user via `system_prompt()`.
struct OAuthPendingAuth {
    /// URL the user needs to visit to authorize.
    auth_url: String,

    /// Receives the authorization code when the callback fires.
    callback_rx: oneshot::Receiver<oauth_callback::CallbackResult>,

    /// Handle to the callback server task (aborted on cleanup).
    callback_handle: JoinHandle<()>,

    /// PKCE code verifier — needed for the token exchange.
    pkce_verifier: String,

    /// The redirect URI used in the auth request (needed for token exchange).
    redirect_uri: String,

    /// Token endpoint URL (from discovery or config override).
    token_url: String,

    /// OAuth client ID (from config or DCR).
    client_id: String,

    /// Optional client secret.
    client_secret: Option<String>,

    /// Server name for the token persistence filename.
    server_name: String,
}

/// Connects to an MCP server and exposes its tools to the agent.
///
/// Created from an `McpConfig` (parsed from dyson.json).  The actual
/// connection and tool discovery happen in `on_load()`.  Before that,
/// `tools()` returns an empty slice.
///
/// ## OAuth support
///
/// When the MCP server is configured with `auth: { "type": "oauth", ... }`,
/// the skill handles the full OAuth 2.0 Authorization Code + PKCE flow:
///
/// 1. Check for persisted tokens at `~/.dyson/tokens/<server>.json`
/// 2. If valid tokens exist → use them (create `OAuthAuth`)
/// 3. If no tokens → start callback server, surface auth URL in system prompt
/// 4. Wait for user to authorize (checked each `before_turn()`)
/// 5. Exchange code for tokens, persist, create `OAuthAuth`
///
/// This is entirely controller-agnostic: the auth URL appears in the system
/// prompt, which the agent relays to the user through whatever controller
/// is active (Terminal, Telegram, etc.).
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

    /// In-progress OAuth flow, if any.  Set during `on_load()` when
    /// OAuth is configured but no persisted tokens exist.  Cleared in
    /// `before_turn()` after the user completes authorization.
    ///
    /// Behind a `Mutex` because `before_turn()` takes `&self` (not `&mut self`)
    /// but needs to complete the OAuth flow and update tools/transport.
    oauth_pending: Mutex<Option<OAuthPendingAuth>>,
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
            oauth_pending: Mutex::new(None),
        }
    }

    /// Create an OAuth-authenticated HTTP transport.
    ///
    /// Tries persisted tokens first.  If none exist (or expired with no
    /// refresh token), runs the OAuth discovery + callback server flow and
    /// sets `self.oauth_pending`.
    async fn create_oauth_transport(
        &mut self,
        server_name: &str,
        url: &str,
        config: &McpAuthConfig,
    ) -> Result<Arc<dyn McpTransport>> {
        let http_client = reqwest::Client::new();

        // 1. Try loading persisted tokens.
        if let Some(credential) = oauth_credential::load_tokens(server_name).await? {
            // If we have a refresh token, we can always get new access tokens.
            // Even if the access token is expired, OAuthAuth handles refresh
            // transparently in apply_to_request().
            if credential.refresh_token.is_some() {
                tracing::info!(
                    server = server_name,
                    expired = credential.is_expired(),
                    "using persisted OAuth tokens"
                );
                let auth = Box::new(OAuthAuth::new(credential));
                return Ok(Arc::new(HttpTransport::new(url, auth)));
            }

            // Access token without refresh token — only usable if not expired.
            if !credential.is_expired() {
                tracing::info!(server = server_name, "using persisted OAuth access token (no refresh token)");
                let auth = Box::new(OAuthAuth::new(credential));
                return Ok(Arc::new(HttpTransport::new(url, auth)));
            }

            tracing::warn!(
                server = server_name,
                "persisted tokens expired with no refresh token — starting new OAuth flow"
            );
        }

        // 2. No usable tokens — run the OAuth flow.

        // 2a. Discover metadata (or use overrides from config).
        let metadata = if config.authorization_url.is_some() && config.token_url.is_some() {
            // Config provides endpoints directly — skip discovery.
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

        // 2b. Get or register a client ID.
        let (client_id, client_secret) = if let Some(ref cid) = config.client_id {
            (cid.clone(), config.client_secret.clone())
        } else {
            // Dynamic Client Registration.
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
                redirect_uris: vec![], // Will be updated after we know the port.
                grant_types: vec![
                    "authorization_code".into(),
                    "refresh_token".into(),
                ],
                response_types: vec!["code".into()],
                token_endpoint_auth_method: Some("none".into()),
            };

            let dcr_response =
                oauth::register_client(reg_url, &dcr_request, &http_client).await?;

            (dcr_response.client_id, dcr_response.client_secret)
        };

        // 2c. Generate PKCE pair.
        let pkce = oauth::generate_pkce();

        // 2d. Start callback server.
        let state = oauth::generate_state();
        let (port, callback_handle, callback_rx) =
            oauth_callback::start_callback_server(&state, Duration::from_secs(300)).await?;

        let redirect_uri = config
            .redirect_uri
            .clone()
            .unwrap_or_else(|| format!("http://127.0.0.1:{port}/callback"));

        // 2e. Build authorization URL.
        let auth_url = oauth::build_auth_url(
            &metadata,
            &client_id,
            &config.scopes,
            &redirect_uri,
            &pkce.challenge,
            &state,
        );

        tracing::info!(
            server = server_name,
            auth_url = %auth_url,
            callback_port = port,
            "OAuth authorization required — waiting for user"
        );

        // 2f. Create placeholder auth and store pending state.
        //
        // The placeholder OAuthAuth has an empty token — it won't work for
        // real requests.  Once the user authorizes (detected in before_turn),
        // the tokens are persisted and the transport is rebuilt.
        let placeholder = OAuthCredential {
            access_token: Credential::new(String::new()),
            refresh_token: None,
            expires_at: std::time::SystemTime::UNIX_EPOCH,
            token_url: metadata.token_endpoint.clone(),
            client_id: client_id.clone(),
            client_secret: client_secret.as_ref().map(|s| Credential::new(s.clone())),
        };

        *self.oauth_pending.lock().await = Some(OAuthPendingAuth {
            auth_url,
            callback_rx,
            callback_handle,
            pkce_verifier: pkce.verifier,
            redirect_uri,
            token_url: metadata.token_endpoint,
            client_id,
            client_secret,
            server_name: server_name.to_string(),
        });

        let auth = Box::new(OAuthAuth::new(placeholder));
        Ok(Arc::new(HttpTransport::new(url, auth)))
    }

    /// Perform the MCP initialization handshake and discover tools.
    ///
    /// Extracted from `on_load()` so it can be called both during initial
    /// load (when tokens exist) and after OAuth completion (in `before_turn`).
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

        // -- Discover tools --
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

        // -- Wrap each tool --
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
    /// ## MCP handshake sequence
    ///
    /// 1. Create transport (stdio or HTTP, with appropriate auth)
    /// 2. Send `initialize` with our client info
    /// 3. Send `initialized` notification (no response expected)
    /// 4. Send `tools/list` to discover available tools
    /// 5. Wrap each tool as an `McpRemoteTool`
    ///
    /// ## OAuth flow (HTTP transport with auth config)
    ///
    /// When the HTTP transport has an OAuth auth config:
    /// 1. Check for persisted tokens at `~/.dyson/tokens/<server>.json`
    /// 2. If valid tokens exist → create `OAuthAuth`, proceed with handshake
    /// 3. If no tokens → run discovery, DCR, start callback server, store
    ///    pending auth state.  The auth URL is surfaced via `system_prompt()`
    ///    and the callback is polled in `before_turn()`.
    async fn on_load(&mut self) -> Result<()> {
        let server_name = self.config.name.clone();

        tracing::info!(server = %server_name, "connecting to MCP server");

        // -- Create the transport --
        //
        // Clone config fields needed for transport creation to avoid holding
        // an immutable borrow on `self` while calling `&mut self` methods.
        let transport: Arc<dyn McpTransport> = match self.config.transport.clone() {
            crate::config::McpTransportConfig::Stdio { command, args, env } => {
                Arc::new(StdioTransport::spawn(&command, &args, &env).await?)
            }
            crate::config::McpTransportConfig::Http {
                url,
                headers,
                auth: None,
            } => {
                // No OAuth — use static headers as before.
                let auth = Box::new(crate::auth::StaticHeadersAuth::new(headers));
                Arc::new(HttpTransport::new(&url, auth))
            }
            crate::config::McpTransportConfig::Http {
                url,
                auth: Some(oauth_config),
                ..
            } => {
                // OAuth configured — try persisted tokens first, then start flow.
                self.create_oauth_transport(&server_name, &url, &oauth_config)
                    .await?
            }
        };

        // If OAuth is pending (no tokens yet), skip the MCP handshake —
        // the auth URL is surfaced via system_prompt() and tools will be
        // discovered after the user authorizes in before_turn().
        {
            let pending = self.oauth_pending.lock().await;
            if let Some(ref p) = *pending {
                self.transport = Some(transport);
                self.system_prompt = Some(format!(
                    "**MCP server '{}' requires authorization.**\n\n\
                     Please visit this URL to authorize:\n{}\n\n\
                     After authorizing, your tools will become available automatically.",
                    server_name, p.auth_url,
                ));
                return Ok(());
            }
        }

        // -- Initialize handshake --
        self.do_mcp_handshake(&server_name, &transport).await?;

        self.transport = Some(transport);
        Ok(())
    }

    /// Called before each LLM turn.
    ///
    /// If an OAuth flow is pending, checks whether the user has completed
    /// authorization (non-blocking check on the callback channel).  If so,
    /// exchanges the code for tokens, persists them, updates the auth, and
    /// runs the MCP handshake to discover tools.
    async fn before_turn(&self) -> Result<Option<String>> {
        let mut pending_guard = self.oauth_pending.lock().await;

        let pending = match pending_guard.take() {
            Some(p) => p,
            None => return Ok(None),
        };

        // Non-blocking check: has the callback fired?
        let mut rx = pending.callback_rx;
        match rx.try_recv() {
            Ok(callback_result) => {
                tracing::info!(
                    server = %pending.server_name,
                    "OAuth callback received — exchanging code for tokens"
                );

                let http_client = reqwest::Client::new();
                let token_response = oauth::exchange_code(
                    &pending.token_url,
                    &callback_result.code,
                    &pending.pkce_verifier,
                    &pending.client_id,
                    pending.client_secret.as_deref(),
                    &pending.redirect_uri,
                    &http_client,
                )
                .await?;

                let oauth_auth = OAuthAuth::from_token_response(
                    &token_response,
                    pending.token_url.clone(),
                    pending.client_id.clone(),
                    pending.client_secret.clone(),
                );

                // Persist tokens for next startup.
                {
                    let cred_guard = oauth_auth.credential().read().await;
                    oauth_credential::persist_tokens(&pending.server_name, &cred_guard).await?;
                }

                // Clean up callback server.
                pending.callback_handle.abort();

                tracing::info!(
                    server = %pending.server_name,
                    "OAuth flow complete — tokens persisted"
                );

                // Return a prompt telling the agent that auth is complete
                // and tools will be available after reconnection.
                // Note: We can't run the MCP handshake here because we
                // only have &self. The tools will be available on the next
                // config reload or restart (persisted tokens will be loaded).
                Ok(Some(format!(
                    "OAuth authorization for MCP server '{}' is complete! \
                     The server's tools will be available after reconnecting. \
                     You may need to ask the user to reload the configuration.",
                    pending.server_name,
                )))
            }
            Err(oneshot::error::TryRecvError::Empty) => {
                // Callback hasn't fired yet — put pending back.
                *pending_guard = Some(OAuthPendingAuth {
                    callback_rx: rx,
                    ..pending
                });

                Ok(Some(format!(
                    "MCP server '{}' is still waiting for OAuth authorization.\n\
                     Visit: {}",
                    pending_guard.as_ref().unwrap().server_name,
                    pending_guard.as_ref().unwrap().auth_url,
                )))
            }
            Err(oneshot::error::TryRecvError::Closed) => {
                pending.callback_handle.abort();
                tracing::warn!(
                    server = %pending.server_name,
                    "OAuth callback timed out"
                );
                Ok(Some(format!(
                    "OAuth authorization for MCP server '{}' timed out. \
                     Please reload the configuration to try again.",
                    pending.server_name,
                )))
            }
        }
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
