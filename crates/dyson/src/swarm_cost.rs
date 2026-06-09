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

    fn audit_list_url(&self, query: &str) -> String {
        let base = self.api_base.trim_end_matches('/');
        if query.is_empty() {
            format!("{base}/audit/calls")
        } else {
            format!("{base}/audit/calls?{query}")
        }
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

/// The cost fields shared verbatim by every swarm cost-row DTO.  Flattened
/// into [`SwarmCostCall`] and [`SwarmAuditCall`] so a field rename can't drift
/// between them; the wire shape is unchanged (the keys serialize flat).
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct CostCore {
    pub audit_id: i64,
    pub provider: String,
    pub model: Option<String>,
    pub key_source: String,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub cost_source: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SwarmCostCall {
    #[serde(flatten)]
    pub core: CostCore,
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

/// One per-request audit row as served by swarm's
/// `/v1/internal/audit/calls`.  Mirrors `RecentCostCall`; re-serialized
/// verbatim to the agent's `/api/audit` so the web UI sees swarm's
/// canonical fields (tok/s, latency, generation id, reconciliation).
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct SwarmAuditCall {
    #[serde(flatten)]
    pub core: CostCore,
    pub status_code: i64,
    pub occurred_at: i64,
    pub total_tokens: Option<i64>,
    #[serde(default)]
    pub ttft_ms: Option<i64>,
    #[serde(default)]
    pub stream_ms: Option<i64>,
    #[serde(default)]
    pub tok_per_sec: Option<f64>,
    #[serde(default)]
    pub upstream_generation_id: Option<String>,
    #[serde(default)]
    pub gen_time_ms: Option<i64>,
    #[serde(default)]
    pub native_output_tokens: Option<i64>,
    #[serde(default)]
    pub reconciled_at: Option<i64>,
}

/// Fetch this instance's recent audit rows from swarm.  `query` is the
/// already-encoded query string (e.g. `range=7d&limit=100`).  Returns an
/// empty vec when no swarm config is wired (standalone dyson).
pub async fn fetch_audit_calls(
    client: &reqwest::Client,
    config: &CostLookupConfig,
    query: &str,
) -> crate::Result<Vec<SwarmAuditCall>> {
    let mut req = client
        .get(config.audit_list_url(query))
        .header("accept", "application/json");
    if let Some(token) = config.bearer.as_deref() {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Vec::new());
    }
    let resp = resp.error_for_status()?;
    Ok(resp.json::<Vec<SwarmAuditCall>>().await?)
}

pub fn metadata_from_cost_call(
    call: SwarmCostCall,
    finalized_at: Option<i64>,
) -> Option<MessageCostMetadata> {
    let core = call.core;
    let cost = core.cost_usd?;
    if !cost.is_finite() {
        return None;
    }
    Some(MessageCostMetadata {
        swarm_llm_audit_id: Some(core.audit_id),
        display_cost_usd: Some(cost),
        cost_source: Some(core.cost_source),
        cost_finalized_at: finalized_at,
        provider: Some(core.provider),
        model: core.model,
        input_tokens: core.input_tokens,
        output_tokens: core.output_tokens,
        key_source: Some(core.key_source),
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
            core: CostCore {
                audit_id: 7,
                provider: "openrouter".into(),
                model: Some("anthropic/claude".into()),
                key_source: "platform".into(),
                input_tokens: Some(10),
                output_tokens: Some(20),
                cost_usd: None,
                cost_source: "missing".into(),
            },
        };
        assert!(metadata_from_cost_call(call, Some(1)).is_none());
    }

    #[test]
    fn cost_call_deserializes_flat_wire_shape() {
        // The flattened `CostCore` must keep the swarm wire keys at the top
        // level — they aren't nested under a `core` object.
        let json = r#"{"audit_id":42,"provider":"openrouter","model":"x","key_source":"platform","input_tokens":1,"output_tokens":2,"cost_usd":0.5,"cost_source":"reported"}"#;
        let call: SwarmCostCall = serde_json::from_str(json).unwrap();
        let meta = metadata_from_cost_call(call, Some(99)).unwrap();
        assert_eq!(meta.swarm_llm_audit_id, Some(42));
        assert_eq!(meta.display_cost_usd, Some(0.5));
    }

    #[test]
    fn audit_call_serializes_flat_wire_shape() {
        let json = r#"{"audit_id":1,"provider":"p","model":null,"key_source":"k","status_code":200,"occurred_at":123,"input_tokens":null,"output_tokens":null,"total_tokens":null,"cost_usd":null,"cost_source":"s"}"#;
        let call: SwarmAuditCall = serde_json::from_str(json).unwrap();
        let out = serde_json::to_value(&call).unwrap();
        // Keys stay flat (frontend reads `r.audit_id`, `r.status_code`, …).
        assert_eq!(out["audit_id"], 1);
        assert_eq!(out["status_code"], 200);
        assert!(out.get("core").is_none(), "core must not nest on the wire");
    }
}
