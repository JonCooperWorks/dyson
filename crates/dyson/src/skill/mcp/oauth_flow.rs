//! OAuth completion shared by callback and submit tool.
use crate::auth::oauth;
use crate::config::McpAuthConfig;
use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

/// Shared state for an in-progress OAuth flow.  Both the background
/// callback task and the oauth_submit tool hold a reference.
pub(super) struct OAuthPending {
    pub(super) server_name: String,
    pub(super) pkce_verifier: String,
    pub(super) redirect_uri: String,
    pub(super) token_endpoint: String,
    pub(super) client_id: String,
    pub(super) client_secret: Option<String>,
    /// RFC 8707 resource indicator sent on the authorize request; must be
    /// echoed on the token exchange when present.
    pub(super) resource: Option<String>,
    pub(super) completed: tokio::sync::Mutex<bool>,
}

impl OAuthPending {
    /// Exchange code for tokens, persist, trigger reload.
    /// Returns Ok(false) if already completed by the other path.
    pub(super) async fn complete(&self, code: &str) -> Result<bool> {
        let mut done = self.completed.lock().await;
        if *done {
            return Ok(false);
        }

        let client = crate::http::client().clone();
        let tokens = oauth::exchange_code(
            &self.token_endpoint,
            code,
            &self.pkce_verifier,
            &self.client_id,
            self.client_secret.as_deref(),
            &self.redirect_uri,
            self.resource.as_deref(),
            &client,
        )
        .await?;

        oauth::persist_tokens(
            &self.server_name,
            &tokens,
            &self.token_endpoint,
            &self.client_id,
            self.client_secret.as_deref(),
        )
        .await?;

        touch_config().await;
        tracing::info!(server = %self.server_name, "OAuth tokens persisted — triggering reload");
        *done = true;
        Ok(true)
    }
}

/// Extract an authorization code from a URL or raw string.
pub(super) fn extract_code(input: &str) -> Option<String> {
    if input.is_empty() {
        return None;
    }
    if input.contains("code=") {
        reqwest::Url::parse(input)
            .ok()
            .and_then(|u| {
                u.query_pairs()
                    .find(|(k, _)| k == "code")
                    .map(|(_, v)| v.into_owned())
            })
            .or_else(|| Some(input.to_string()))
    } else {
        Some(input.to_string())
    }
}

/// Temporary tool for manual OAuth code submission (NAT fallback).
pub(super) struct OAuthSubmitTool {
    pub(super) pending: Arc<OAuthPending>,
    pub(super) tool_name: String,
}

#[async_trait]
impl Tool for OAuthSubmitTool {
    fn name(&self) -> &str {
        &self.tool_name
    }
    fn description(&self) -> &str {
        "Submit an OAuth authorization code or redirect URL to complete authentication."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "code_or_url": { "type": "string" } },
            "required": ["code_or_url"]
        })
    }

    async fn run(&self, input: &serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let Some(code) = extract_code(input["code_or_url"].as_str().unwrap_or("")) else {
            return Ok(ToolOutput::error("No authorization code found."));
        };

        match self.pending.complete(&code).await {
            Ok(true) => Ok(ToolOutput::success(format!(
                "OAuth complete for '{}'. Reconnecting...",
                self.pending.server_name
            ))),
            Ok(false) => Ok(ToolOutput::success("Already authorized.")),
            Err(e) => Ok(ToolOutput::error(format!("Token exchange failed: {e}"))),
        }
    }
}

async fn touch_config() {
    let path = std::env::args()
        .skip_while(|a| a != "--config" && a != "-c")
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("dyson.json"));
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(f) = std::fs::File::options().write(true).open(&path) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
    })
    .await;
}

pub(super) async fn start_oauth_flow(
    server_name: &str,
    url: &str,
    config: &McpAuthConfig,
) -> Result<(String, Arc<dyn Tool>)> {
    let http_client = crate::http::client().clone();

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
        let reg_url = config
            .registration_url
            .as_deref()
            .or(meta.registration_endpoint.as_deref())
            .ok_or_else(|| {
                DysonError::oauth(server_name, "no client_id and no registration endpoint")
            })?;
        let dcr = oauth::register_client(
            reg_url,
            &oauth::DcrRequest {
                client_name: format!("dyson-{server_name}"),
                redirect_uris: vec![],
                grant_types: vec!["authorization_code".into(), "refresh_token".into()],
                response_types: vec!["code".into()],
                token_endpoint_auth_method: Some("none".into()),
                // The shared DTO carries an optional scope (swarm/Smithery
                // need it); dyson requests scopes on the authorize URL, not
                // at registration, so it stays None here.
                scope: None,
            },
            &http_client,
        )
        .await?;
        (dcr.client_id, dcr.client_secret)
    };

    let pkce = oauth::generate_pkce();
    let state = oauth::generate_state();
    let (port, callback_handle, callback_rx) =
        oauth::start_callback_server(&state, Duration::from_secs(300)).await?;

    // RFC 8707 resource indicator: the MCP server's own origin. Required by
    // ASes that bind tokens to a resource (this fleet's exposure AS returns
    // invalid_target without it); ignored by ASes that don't use it.
    let resource = oauth::canonical_resource(url);

    let redirect_uri = config
        .redirect_uri
        .clone()
        .unwrap_or_else(|| format!("http://127.0.0.1:{port}/callback"));
    let auth_url = oauth::build_auth_url(
        &meta.authorization_endpoint,
        &client_id,
        &config.scopes,
        &redirect_uri,
        &pkce.challenge,
        &state,
        resource.as_deref(),
    )?;

    let pending = Arc::new(OAuthPending {
        server_name: server_name.to_string(),
        pkce_verifier: pkce.verifier,
        redirect_uri,
        token_endpoint: meta.token_endpoint,
        client_id,
        client_secret,
        resource,
        completed: tokio::sync::Mutex::new(false),
    });

    let bg = Arc::clone(&pending);
    tokio::spawn(async move {
        let result = async {
            let code = callback_rx
                .await
                .map_err(|_| DysonError::oauth(&bg.server_name, "callback channel closed"))?;
            bg.complete(&code).await?;
            Ok::<(), DysonError>(())
        }
        .await;
        callback_handle.abort();
        if let Err(e) = result {
            tracing::warn!(server = %bg.server_name, error = %e, "OAuth background failed");
        }
    });

    let tool: Arc<dyn Tool> = Arc::new(OAuthSubmitTool {
        tool_name: format!("{server_name}_oauth_submit"),
        pending,
    });
    Ok((auth_url, tool))
}

pub(super) async fn load_oauth_credential(
    server_name: &str,
) -> Result<Option<Box<dyn crate::auth::Auth>>> {
    if let Some(cred) = oauth::load_tokens(server_name).await? {
        if cred.refresh_token.is_some() || !cred.is_expired() {
            tracing::info!(server = server_name, "using persisted OAuth tokens");
            return Ok(Some(Box::new(oauth::OAuth::new(cred))));
        }
        tracing::warn!(
            server = server_name,
            "persisted tokens expired with no refresh token"
        );
    }
    Ok(None)
}
