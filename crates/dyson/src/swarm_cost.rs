//! Display-only lookup of Swarm's canonical LLM cost rows.
//!
//! Dyson stores these fields on assistant messages only so the chat UI can
//! render a price. Swarm remains the billing/accounting source of truth.

use std::sync::{OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::message::MessageCostMetadata;

const ENV_PROXY_URL: &str = "SWARM_PROXY_URL";
const ENV_PROXY_TOKEN: &str = "SWARM_PROXY_TOKEN";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostLookupConfig {
    api_base: String,
    bearer: Option<String>,
}

impl CostLookupConfig {
    pub fn public_api(swarm_url: &str, bearer: Option<&str>) -> Option<Self> {
        let base = public_api_base(swarm_url)?;
        Some(Self {
            api_base: base,
            bearer: bearer
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        })
    }

    pub fn internal_from_proxy(proxy_url: &str, proxy_token: &str) -> Option<Self> {
        let proxy_token = proxy_token.trim();
        if proxy_token.is_empty() {
            return None;
        }
        Some(Self {
            api_base: internal_api_base_from_proxy_url(proxy_url).ok()?,
            bearer: Some(proxy_token.to_string()),
        })
    }

    fn from_env() -> Option<Self> {
        Self::internal_from_proxy(
            &std::env::var(ENV_PROXY_URL).ok()?,
            &std::env::var(ENV_PROXY_TOKEN).ok()?,
        )
    }

    fn call_url(&self, audit_id: i64) -> String {
        format!(
            "{}/costs/calls/{audit_id}",
            self.api_base.trim_end_matches('/')
        )
    }
}

static RUNTIME_CONFIG: OnceLock<RwLock<Option<CostLookupConfig>>> = OnceLock::new();

fn runtime_config() -> &'static RwLock<Option<CostLookupConfig>> {
    RUNTIME_CONFIG.get_or_init(|| RwLock::new(None))
}

pub fn set_runtime_config(config: Option<CostLookupConfig>) {
    if let Ok(mut guard) = runtime_config().write() {
        *guard = config;
    }
}

pub fn set_runtime_config_from_parts(proxy_url: &str, proxy_token: &str) {
    set_runtime_config(CostLookupConfig::internal_from_proxy(
        proxy_url,
        proxy_token,
    ));
}

pub fn config_snapshot() -> Option<CostLookupConfig> {
    runtime_config().read().ok().and_then(|guard| guard.clone())
}

pub fn config_snapshot_or_env() -> Option<CostLookupConfig> {
    config_snapshot().or_else(CostLookupConfig::from_env)
}

pub async fn lookup_runtime_display_metadata(
    audit_id: i64,
) -> crate::Result<Option<MessageCostMetadata>> {
    let Some(config) = config_snapshot_or_env() else {
        return Ok(None);
    };
    let Some(call) = fetch_cost_call(crate::http::client(), &config, audit_id).await? else {
        return Ok(None);
    };
    Ok(metadata_from_cost_call(call, Some(now_secs())))
}

#[derive(Debug, Clone, Deserialize)]
pub struct SwarmCostCall {
    pub audit_id: i64,
    pub provider: String,
    pub model: Option<String>,
    pub key_source: String,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub cost_source: String,
}

pub async fn fetch_cost_call(
    client: &reqwest::Client,
    config: &CostLookupConfig,
    audit_id: i64,
) -> crate::Result<Option<SwarmCostCall>> {
    let mut req = client
        .get(config.call_url(audit_id))
        .header("accept", "application/json");
    if let Some(token) = config.bearer.as_deref() {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let resp = resp.error_for_status()?;
    Ok(Some(resp.json::<SwarmCostCall>().await?))
}

pub fn metadata_from_cost_call(
    call: SwarmCostCall,
    finalized_at: Option<i64>,
) -> Option<MessageCostMetadata> {
    let cost = call.cost_usd?;
    if !cost.is_finite() {
        return None;
    }
    Some(MessageCostMetadata {
        swarm_llm_audit_id: Some(call.audit_id),
        display_cost_usd: Some(cost),
        cost_source: Some(call.cost_source),
        cost_finalized_at: finalized_at,
        provider: Some(call.provider),
        model: call.model,
        input_tokens: call.input_tokens,
        output_tokens: call.output_tokens,
        key_source: Some(call.key_source),
    })
}

fn public_api_base(swarm_url: &str) -> Option<String> {
    let trimmed = swarm_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.ends_with("/v1") || trimmed.ends_with("/v1/internal") {
        return Some(trimmed.to_string());
    }
    Some(format!("{trimmed}/v1"))
}

fn internal_api_base_from_proxy_url(proxy_url: &str) -> Result<String, String> {
    let trimmed = proxy_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("empty URL".into());
    }
    if let Some(prefix) = trimmed.strip_suffix("/v1/internal") {
        return Ok(format!("{prefix}/v1/internal"));
    }
    if let Some(prefix) = trimmed.strip_suffix("/v1") {
        return Ok(format!("{prefix}/v1/internal"));
    }
    if let Some((prefix, _)) = trimmed.split_once("/llm") {
        return Ok(format!("{prefix}/v1/internal"));
    }
    Ok(format!("{trimmed}/v1/internal"))
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bases_are_derived_from_proxy_and_public_urls() {
        assert_eq!(
            internal_api_base_from_proxy_url("https://swarm.test/llm/openrouter").unwrap(),
            "https://swarm.test/v1/internal"
        );
        assert_eq!(
            internal_api_base_from_proxy_url("https://swarm.test/v1").unwrap(),
            "https://swarm.test/v1/internal"
        );
        assert_eq!(
            public_api_base("https://swarm.test").unwrap(),
            "https://swarm.test/v1"
        );
        assert_eq!(
            public_api_base("https://swarm.test/v1").unwrap(),
            "https://swarm.test/v1"
        );
    }

    #[test]
    fn metadata_requires_real_cost() {
        let call = SwarmCostCall {
            audit_id: 7,
            provider: "openrouter".into(),
            model: Some("anthropic/claude".into()),
            key_source: "platform".into(),
            input_tokens: Some(10),
            output_tokens: Some(20),
            cost_usd: None,
            cost_source: "missing".into(),
        };
        assert!(metadata_from_cost_call(call, Some(1)).is_none());
    }
}
