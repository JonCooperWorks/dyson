// ===========================================================================
// MCP skill — connect to MCP servers and expose their tools to the agent.
//
// Module layout:
//   mod.rs        — McpSkill (Skill impl) + McpRemoteTool (Tool impl)
//   protocol.rs   — JSON-RPC message types
//   transport.rs  — Stdio + HTTP transports
//   serve.rs      — HTTP MCP server (Dyson serves tools TO Claude Code)
//
// OAuth flow (non-blocking):
//   No tokens → start callback server in background, register a temporary
//   oauth_submit tool, show auth URL in system prompt.  The user either:
//   (a) clicks the URL and the callback server receives the code, or
//   (b) pastes the redirect URL into the chat and the agent calls oauth_submit.
//   Either way, tokens are persisted and the config is auto-touched to
//   trigger a hot reload.
// ===========================================================================

pub mod protocol;
pub mod serve;
pub mod transport;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::auth::oauth;
use crate::config::{McpAuthConfig, McpConfig};
use crate::error::{DysonError, Result};
use crate::skill::Skill;
use crate::tool::{Tool, ToolContext, ToolOutput};

use self::protocol::{McpContent, McpToolDef, McpToolResult};
use self::transport::{HttpTransport, McpTransport, StdioTransport};

/// Shared state for an in-progress OAuth flow.  Both the background callback
/// task and the `oauth_submit` tool hold a reference.  Whichever receives
/// the code first completes the exchange; the `completed` flag prevents
/// double-exchange.
struct OAuthPending {
    server_name: String,
    pkce_verifier: String,
    redirect_uri: String,
    token_endpoint: String,
    client_id: String,
    client_secret: Option<String>,
    completed: tokio::sync::Mutex<bool>,
}

impl OAuthPending {
    /// Exchange an authorization code for tokens, persist, and trigger reload.
    /// Returns Ok(true) if we did the exchange, Ok(false) if already completed.
    async fn complete(&self, code: &str) -> Result<bool> {
        let mut done = self.completed.lock().await;
        if *done { return Ok(false); }

        let http_client = reqwest::Client::new();
        let tokens = oauth::exchange_code(
            &self.token_endpoint, code, &self.pkce_verifier,
            &self.client_id, self.client_secret.as_deref(),
            &self.redirect_uri, &http_client,
        ).await?;

        oauth::persist_tokens(
            &self.server_name, &tokens, &self.token_endpoint,
            &self.client_id, self.client_secret.as_deref(),
        ).await?;

        touch_config().await;
        tracing::info!(server = %self.server_name, "OAuth tokens persisted — triggering reload");

        *done = true;
        Ok(true)
    }
}

/// Temporary tool registered when an OAuth flow is pending.  Accepts
/// either a full redirect URL or a raw authorization code.  Lets users
/// behind NAT paste the redirect URL from their browser.
struct OAuthSubmitTool {
    pending: Arc<OAuthPending>,
    tool_name: String,
}

#[async_trait]
impl Tool for OAuthSubmitTool {
    fn name(&self) -> &str { &self.tool_name }

    fn description(&self) -> &str {
        "Submit an OAuth authorization code or redirect URL to complete authentication. \
         Call this when the user pastes a URL containing ?code=... or provides a raw code."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "code_or_url": {
                    "type": "string",
                    "description": "The authorization code, or the full redirect URL containing ?code=..."
                }
            },
            "required": ["code_or_url"]
        })
    }

    async fn run(&self, input: &serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let input_str = input["code_or_url"].as_str().unwrap_or("");

        // Extract code from a URL or use as raw code.
        let code = if input_str.contains("code=") {
            reqwest::Url::parse(input_str)
                .ok()
                .and_then(|u| {
                    u.query_pairs()
                        .find(|(k, _)| k == "code")
                        .map(|(_, v)| v.into_owned())
                })
                .unwrap_or_else(|| input_str.to_string())
        } else {
            input_str.to_string()
        };

        if code.is_empty() {
            return Ok(ToolOutput {
                content: "No authorization code found. Please provide the code or the full redirect URL.".into(),
                is_error: true, metadata: None, files: vec![],
            });
        }

        match self.pending.complete(&code).await {
            Ok(true) => Ok(ToolOutput {
                content: format!(
                    "OAuth authorization complete for '{}'. The server will reconnect automatically.",
                    self.pending.server_name
                ),
                is_error: false, metadata: None, files: vec![],
            }),
            Ok(false) => Ok(ToolOutput {
                content: "Authorization was already completed.".into(),
                is_error: false, metadata: None, files: vec![],
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("OAuth token exchange failed: {e}"),
                is_error: true, metadata: None, files: vec![],
            }),
        }
    }
}

pub struct McpSkill {
    config: McpConfig,
    transport: Option<Arc<dyn McpTransport>>,
    tools: Vec<Arc<dyn Tool>>,
    system_prompt: Option<String>,
}

impl McpSkill {
    pub fn new(config: McpConfig) -> Self {
        Self { config, transport: None, tools: Vec::new(), system_prompt: None }
    }

    /// Start background OAuth flow.  Returns `(auth_url, submit_tool)`.
    ///
    /// The background task waits for the callback server; the submit tool
    /// lets users paste the redirect URL manually.  Both share `OAuthPending`
    /// so whichever gets the code first wins.
    async fn start_oauth_flow(
        server_name: &str, url: &str, config: &McpAuthConfig,
    ) -> Result<(String, Arc<dyn Tool>)> {
        let http_client = reqwest::Client::new();

        let meta = if let (Some(a), Some(t)) = (&config.authorization_url, &config.token_url) {
            oauth::AuthMetadata {
                authorization_endpoint: a.clone(),
                token_endpoint: t.clone(),
                registration_endpoint: config.registration_url.clone(),
            }
        } else {
            oauth::discover_metadata(url, &http_client).await?
        };

        let (client_id, client_secret) = if let Some(ref cid) = config.client_id {
            (cid.clone(), config.client_secret.clone())
        } else {
            let reg_url = config.registration_url.as_deref()
                .or(meta.registration_endpoint.as_deref())
                .ok_or_else(|| DysonError::oauth(server_name, "no client_id and no registration endpoint"))?;
            let dcr = oauth::register_client(reg_url, &oauth::DcrRequest {
                client_name: format!("dyson-{server_name}"),
                redirect_uris: vec![],
                grant_types: vec!["authorization_code".into(), "refresh_token".into()],
                response_types: vec!["code".into()],
                token_endpoint_auth_method: Some("none".into()),
            }, &http_client).await?;
            (dcr.client_id, dcr.client_secret)
        };

        let pkce = oauth::generate_pkce();
        let state = oauth::generate_state();

        let (port, callback_handle, callback_rx) =
            oauth::start_callback_server(&state, Duration::from_secs(300)).await?;

        let redirect_uri = config.redirect_uri.clone()
            .unwrap_or_else(|| format!("http://127.0.0.1:{port}/callback"));

        let auth_url = oauth::build_auth_url(
            &meta.authorization_endpoint, &client_id, &config.scopes,
            &redirect_uri, &pkce.challenge, &state,
        );

        let pending = Arc::new(OAuthPending {
            server_name: server_name.to_string(),
            pkce_verifier: pkce.verifier,
            redirect_uri,
            token_endpoint: meta.token_endpoint,
            client_id,
            client_secret,
            completed: tokio::sync::Mutex::new(false),
        });

        // Background task: wait for callback server to receive the code.
        let bg_pending = Arc::clone(&pending);
        tokio::spawn(async move {
            let result = async {
                let code = callback_rx.await.map_err(|_| {
                    DysonError::oauth(&bg_pending.server_name, "callback channel closed")
                })?;
                bg_pending.complete(&code).await?;
                Ok::<(), DysonError>(())
            }.await;

            callback_handle.abort();
            if let Err(e) = result {
                tracing::warn!(server = %bg_pending.server_name, error = %e, "OAuth background flow failed");
            }
        });

        let tool: Arc<dyn Tool> = Arc::new(OAuthSubmitTool {
            tool_name: format!("{}_oauth_submit", server_name),
            pending,
        });

        Ok((auth_url, tool))
    }

    async fn load_oauth_auth(server_name: &str) -> Result<Option<Box<dyn crate::auth::Auth>>> {
        if let Some(cred) = oauth::load_tokens(server_name).await? {
            if cred.refresh_token.is_some() || !cred.is_expired() {
                tracing::info!(server = server_name, "using persisted OAuth tokens");
                return Ok(Some(Box::new(oauth::OAuth::new(cred))));
            }
            tracing::warn!(server = server_name, "persisted tokens expired with no refresh token");
        }
        Ok(None)
    }

    async fn do_mcp_handshake(&mut self, server_name: &str, transport: &Arc<dyn McpTransport>) -> Result<()> {
        let init_params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "dyson", "version": env!("CARGO_PKG_VERSION") }
        });

        let init_result = transport.send_request("initialize", Some(init_params)).await?;
        tracing::debug!(server = server_name, result = %init_result, "MCP initialize response");

        transport.send_notification("notifications/initialized", None).await?;

        let tools_result = transport.send_request("tools/list", Some(serde_json::json!({}))).await?;
        let tool_defs: Vec<McpToolDef> = match tools_result.get("tools") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| DysonError::Mcp {
                server: server_name.to_string(), message: format!("failed to parse tools/list: {e}"),
            })?,
            None => vec![],
        };

        tracing::info!(server = server_name, tool_count = tool_defs.len(), "MCP tools discovered");

        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut descs: Vec<String> = Vec::new();

        for def in tool_defs {
            let desc = def.description.clone().unwrap_or_default();
            descs.push(format!("- **{}**: {}", def.name, desc));
            tools.push(Arc::new(McpRemoteTool {
                tool_name: def.name, description: desc,
                input_schema: def.input_schema.unwrap_or(serde_json::json!({"type": "object"})),
                transport: Arc::clone(transport), server_name: server_name.to_string(),
            }));
        }

        self.tools = tools;
        if !descs.is_empty() {
            self.system_prompt = Some(format!("MCP server '{}' provides these tools:\n{}", server_name, descs.join("\n")));
        }
        Ok(())
    }
}

#[async_trait]
impl Skill for McpSkill {
    fn name(&self) -> &str { &self.config.name }
    fn tools(&self) -> &[Arc<dyn Tool>] { &self.tools }
    fn system_prompt(&self) -> Option<&str> { self.system_prompt.as_deref() }

    async fn on_load(&mut self) -> Result<()> {
        let server_name = self.config.name.clone();
        tracing::info!(server = %server_name, "connecting to MCP server");

        let transport: Option<Arc<dyn McpTransport>> = match self.config.transport.clone() {
            crate::config::McpTransportConfig::Stdio { command, args, env } => {
                Some(Arc::new(StdioTransport::spawn(&command, &args, &env).await?))
            }
            crate::config::McpTransportConfig::Http { url, headers, auth: None } => {
                Some(Arc::new(HttpTransport::new(&url, Box::new(crate::auth::StaticHeadersAuth::new(headers)))))
            }
            crate::config::McpTransportConfig::Http { url, auth: Some(oauth_config), .. } => {
                match Self::load_oauth_auth(&server_name).await? {
                    Some(auth) => Some(Arc::new(HttpTransport::new(&url, auth))),
                    None => {
                        let (auth_url, submit_tool) = Self::start_oauth_flow(
                            &server_name, &url, &oauth_config,
                        ).await?;

                        self.tools = vec![submit_tool];
                        self.system_prompt = Some(format!(
                            "**MCP server '{server_name}' requires OAuth authorization.**\n\n\
                             Tell the user to open this URL:\n{auth_url}\n\n\
                             If the callback works automatically, the server will reconnect.\n\
                             If not (e.g. user is remote), ask them to paste the redirect URL \
                             they were sent to after authorizing, then call {server_name}_oauth_submit \
                             with it.",
                        ));
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

async fn touch_config() {
    let path = std::env::args()
        .skip_while(|a| a != "--config" && a != "-c")
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("dyson.json"));

    let _ = tokio::task::spawn_blocking(move || {
        match std::fs::File::options().write(true).open(&path) {
            Ok(f) => { let _ = f.set_modified(std::time::SystemTime::now()); }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e,
                    "could not touch config — reload manually to connect OAuth server");
            }
        }
    }).await;
}

struct McpRemoteTool {
    tool_name: String,
    description: String,
    input_schema: serde_json::Value,
    transport: Arc<dyn McpTransport>,
    server_name: String,
}

#[async_trait]
impl Tool for McpRemoteTool {
    fn name(&self) -> &str { &self.tool_name }
    fn description(&self) -> &str { &self.description }
    fn input_schema(&self) -> serde_json::Value { self.input_schema.clone() }

    async fn run(&self, input: &serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let params = serde_json::json!({ "name": self.tool_name, "arguments": input });

        let result_json = self.transport
            .send_request("tools/call", Some(params))
            .await
            .map_err(|e| DysonError::Mcp {
                server: self.server_name.clone(),
                message: format!("tools/call failed for '{}': {e}", self.tool_name),
            })?;

        let tool_result: McpToolResult = serde_json::from_value(result_json)
            .map_err(|e| DysonError::Mcp {
                server: self.server_name.clone(),
                message: format!("failed to parse tools/call result: {e}"),
            })?;

        let content: String = tool_result.content.iter()
            .filter_map(|c| match c { McpContent::Text { text } => Some(text.as_str()), _ => None })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolOutput { content, is_error: tool_result.is_error, metadata: None, files: vec![] })
    }
}
