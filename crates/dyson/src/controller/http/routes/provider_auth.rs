//! Thin SPA relay for Swarm-owned provider subscription authentication.
//!
//! Provider OAuth credentials never cross this boundary. Dyson authenticates
//! to Swarm with its source-IP-bound `pt_` proxy token and relays only coarse
//! connection state, one-time browser URLs/codes, and disconnect requests.

use hyper::{Method, Request, StatusCode};
use serde::Deserialize;

use crate::config::LlmProvider;

use super::super::responses::{Resp, bad_request, json_status, not_found, read_json_capped};
use super::super::state::HttpState;

const MAX_COMPLETE_BODY: usize = 16 * 1024;

#[derive(Clone, Copy)]
pub(super) enum SubscriptionProvider {
    Codex,
    Claude,
}

impl SubscriptionProvider {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    const fn provider_type(self) -> LlmProvider {
        match self {
            Self::Codex => LlmProvider::Codex,
            Self::Claude => LlmProvider::ClaudeCode,
        }
    }
}

#[derive(Deserialize, serde::Serialize)]
struct CompleteRequest {
    code: String,
}

fn provider_enabled(state: &HttpState, provider: SubscriptionProvider) -> bool {
    state
        .settings_snapshot()
        .providers
        .values()
        .any(|configured| configured.provider_type == provider.provider_type())
}

fn endpoint(provider: SubscriptionProvider, suffix: &str) -> Result<(String, String), String> {
    let (proxy_url, token) = crate::swarm_cost::runtime_proxy_parts()
        .ok_or_else(|| "managed subscription sign-in is unavailable".to_owned())?;
    endpoint_from_parts(&proxy_url, &token, provider, suffix)
}

fn endpoint_from_parts(
    proxy_url: &str,
    token: &str,
    provider: SubscriptionProvider,
    suffix: &str,
) -> Result<(String, String), String> {
    if token.trim().is_empty() {
        return Err("managed subscription sign-in is unavailable".to_owned());
    }
    let base = crate::swarm_cost::internal_api_base_from_proxy_url(proxy_url)?;
    Ok((
        format!(
            "{}/subscription-auth/{}{}",
            base.trim_end_matches('/'),
            provider.as_str(),
            suffix
        ),
        token.trim().to_owned(),
    ))
}

async fn relay(
    state: &HttpState,
    provider: SubscriptionProvider,
    method: reqwest::Method,
    suffix: &str,
    body: Option<CompleteRequest>,
) -> Resp {
    if !provider_enabled(state, provider) {
        return not_found();
    }
    let (url, token) = match endpoint(provider, suffix) {
        Ok(parts) => parts,
        Err(message) => {
            return json_status(
                StatusCode::SERVICE_UNAVAILABLE,
                &serde_json::json!({ "error": message }),
            );
        }
    };
    let mut request = crate::http::client()
        .request(method, url)
        .bearer_auth(token)
        .header("accept", "application/json");
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%error, provider = provider.as_str(), "Swarm subscription broker unavailable");
            return json_status(
                StatusCode::SERVICE_UNAVAILABLE,
                &serde_json::json!({ "error": "Swarm subscription broker is unavailable" }),
            );
        }
    };
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let value = match response.json::<serde_json::Value>().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, provider = provider.as_str(), "invalid Swarm subscription response");
            return json_status(
                StatusCode::BAD_GATEWAY,
                &serde_json::json!({ "error": "Swarm returned an invalid subscription response" }),
            );
        }
    };
    json_status(status, &value)
}

pub(super) async fn status(state: &HttpState, provider: SubscriptionProvider) -> Resp {
    relay(state, provider, reqwest::Method::GET, "", None).await
}

pub(super) async fn start(state: &HttpState, provider: SubscriptionProvider) -> Resp {
    relay(state, provider, reqwest::Method::POST, "", None).await
}

pub(super) async fn forget(state: &HttpState, provider: SubscriptionProvider) -> Resp {
    relay(state, provider, reqwest::Method::DELETE, "", None).await
}

pub(super) async fn complete(
    req: Request<hyper::body::Incoming>,
    state: &HttpState,
    provider: SubscriptionProvider,
) -> Resp {
    if req.method() != Method::POST {
        return not_found();
    }
    let body: CompleteRequest =
        match read_json_capped::<CompleteRequest>(req, MAX_COMPLETE_BODY).await {
            Ok(body) if !body.code.trim().is_empty() => body,
            Ok(_) => return bad_request("authorization code is required"),
            Err(error) => return bad_request(&error),
        };
    relay(
        state,
        provider,
        reqwest::Method::POST,
        "/complete",
        Some(body),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ResetRuntimeConfig;

    impl Drop for ResetRuntimeConfig {
        fn drop(&mut self) {
            crate::swarm_cost::set_runtime_config(None);
        }
    }

    #[test]
    fn provider_endpoint_never_contains_provider_credentials() {
        let (url, token) = endpoint_from_parts(
            "https://swarm.test/llm",
            "pt_instance_only",
            SubscriptionProvider::Claude,
            "/complete",
        )
        .unwrap();
        assert_eq!(
            url,
            "https://swarm.test/v1/internal/subscription-auth/claude/complete"
        );
        assert_eq!(token, "pt_instance_only");
    }

    #[tokio::test]
    async fn provider_endpoint_uses_post_warmup_runtime_patch() {
        let _guard = crate::swarm_cost::test_config_guard().await;
        let _reset = ResetRuntimeConfig;
        crate::swarm_cost::set_runtime_config_from_parts(
            "http://169.254.68.5:8080/llm",
            "pt_runtime_only",
        );

        let (url, token) = endpoint(SubscriptionProvider::Codex, "").unwrap();
        assert_eq!(
            url,
            "http://169.254.68.5:8080/v1/internal/subscription-auth/codex"
        );
        assert_eq!(token, "pt_runtime_only");
    }
}
