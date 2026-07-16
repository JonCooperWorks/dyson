// ===========================================================================
// /api/models — the full model *catalogue* the active provider can reach.
//
// Distinct from /api/providers (which lists only the models configured in
// dyson.json).  A managed dyson now boots with a single seeded model, so the
// in-UI switcher needs somewhere to discover everything else it could run —
// this route proxies the active provider's OpenAI-compatible `/v1/models`
// endpoint (for the swarm that's `<proxy>/openrouter/v1/models`, i.e. the
// real OpenRouter catalogue through the metered proxy) and normalizes it.
//
// `POST /api/model` already accepts an arbitrary model id, so a pick here
// switches end-to-end with no allowlist to maintain.
//
// Degrades to `{ "models": [] }` (never a 5xx) whenever no catalogue is
// reachable — off-swarm dev, a provider with no base_url, or an upstream
// blip — so the SPA can quietly fall back to the configured list.
// ===========================================================================

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::super::responses::{Resp, json_ok};
use super::super::state::HttpState;

/// One normalized catalogue entry.  Only `id` is guaranteed; everything
/// else is best-effort from whatever the upstream `/v1/models` payload
/// carried, so the SPA renders a bare id when metadata is absent.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct ModelEntry {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completion_price: Option<String>,
}

/// In-process catalogue cache.  The catalogue changes on the order of days,
/// so a single-slot cache keyed on the base_url it was fetched from (the
/// operator can switch providers under us) with a 30-minute TTL keeps this
/// route from hammering the proxy on every menu open — mirrors the swarm's
/// own model-list TTL.
struct CacheEntry {
    base_url: String,
    fetched_at: Instant,
    models: Vec<ModelEntry>,
}

static CACHE: OnceLock<Mutex<Option<CacheEntry>>> = OnceLock::new();
const CACHE_TTL: Duration = Duration::from_secs(30 * 60);

fn cache() -> &'static Mutex<Option<CacheEntry>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

/// The catalogue URL for a provider, or `None` when the provider can't
/// reach one (no base_url configured — i.e. off-swarm / CLI providers).
///
/// The active provider's `base_url` stops at `/openrouter`; the OpenAI-
/// compatible client appends `/v1/chat/completions` for chat, so the
/// sibling catalogue endpoint is `<base_url>/v1/models`.
fn catalogue_url(base_url: Option<&str>) -> Option<String> {
    let base = base_url.unwrap_or("").trim().trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    Some(format!("{base}/v1/models"))
}

/// Coerce a pricing field to a string.  OpenRouter sends per-token prices
/// as decimal strings ("0.0000015"); tolerate a bare number too.
fn price_string(v: Option<&serde_json::Value>) -> Option<String> {
    match v {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// Normalize an OpenAI-compatible `/v1/models` body (`{ data: [...] }`)
/// into our bounded entry shape.  Skips entries without a string `id`.
fn normalize(body: &serde_json::Value) -> Vec<ModelEntry> {
    let Some(data) = body.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    data.iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(|v| v.as_str())?.trim();
            if id.is_empty() {
                return None;
            }
            let name = m
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let context_length = m
                .get("context_length")
                .and_then(|v| v.as_u64())
                .filter(|&n| n > 0);
            let pricing = m.get("pricing");
            let prompt_price = price_string(pricing.and_then(|p| p.get("prompt")));
            let completion_price = price_string(pricing.and_then(|p| p.get("completion")));
            Some(ModelEntry {
                id: id.to_owned(),
                name,
                context_length,
                prompt_price,
                completion_price,
            })
        })
        .collect()
}

pub(super) async fn list(state: &HttpState) -> Resp {
    // Resolve the active provider the same way /api/providers does: a
    // runtime override (from POST /api/model) wins over the startup
    // active-provider so the catalogue is fetched through whichever
    // provider the next turn will actually use.
    let snapshot = state.settings_snapshot();
    let runtime = state.runtime_model.lock().ok().and_then(|g| g.clone());
    let active_name = runtime
        .as_ref()
        .map(|selection| selection.provider().to_string())
        .or_else(|| crate::controller::active_provider_name(&snapshot));

    let Some(provider_cfg) = active_name
        .as_deref()
        .and_then(|name| snapshot.providers.get(name))
    else {
        return json_ok(&serde_json::json!({ "models": [] }));
    };
    let Some(url) = catalogue_url(provider_cfg.base_url.as_deref()) else {
        // Off-swarm / CLI provider with no HTTP catalogue — degrade.
        return json_ok(&serde_json::json!({ "models": [] }));
    };

    // Serve a fresh cache hit without touching the network.
    if let Ok(guard) = cache().lock()
        && let Some(entry) = guard.as_ref()
        && entry.base_url == url
        && entry.fetched_at.elapsed() < CACHE_TTL
    {
        return json_ok(&serde_json::json!({ "models": entry.models }));
    }

    let key = provider_cfg.api_key.expose().to_string();
    let fetched = crate::http::client()
        .get(&url)
        .bearer_auth(&key)
        .send()
        .await;

    let models = match fetched {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(body) => Some(normalize(&body)),
            Err(e) => {
                tracing::debug!(error = %e, "model catalogue: upstream body was not JSON");
                None
            }
        },
        Ok(resp) => {
            tracing::debug!(status = %resp.status(), "model catalogue: upstream non-2xx");
            None
        }
        Err(e) => {
            tracing::debug!(error = %e, "model catalogue: upstream request failed");
            None
        }
    };

    match models {
        Some(models) => {
            if let Ok(mut guard) = cache().lock() {
                *guard = Some(CacheEntry {
                    base_url: url,
                    fetched_at: Instant::now(),
                    models: models.clone(),
                });
            }
            json_ok(&serde_json::json!({ "models": models }))
        }
        // Upstream failed and there's no fresh cache — fall back to stale
        // cache for the same base_url if we have one, else an empty list.
        None => {
            let stale = cache().lock().ok().and_then(|g| {
                g.as_ref()
                    .filter(|e| e.base_url == url)
                    .map(|e| e.models.clone())
            });
            json_ok(&serde_json::json!({ "models": stale.unwrap_or_default() }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_url_appends_v1_models() {
        assert_eq!(
            catalogue_url(Some("https://swarm.example/llm/openrouter")).as_deref(),
            Some("https://swarm.example/llm/openrouter/v1/models"),
        );
        // Trailing slash is trimmed so we never double up on `//`.
        assert_eq!(
            catalogue_url(Some("https://swarm.example/llm/openrouter/")).as_deref(),
            Some("https://swarm.example/llm/openrouter/v1/models"),
        );
    }

    #[test]
    fn catalogue_url_is_none_off_swarm() {
        // No base_url (CLI provider / off-swarm dev) → no catalogue, and
        // the route degrades to an empty list instead of 500ing.
        assert_eq!(catalogue_url(None), None);
        assert_eq!(catalogue_url(Some("")), None);
        assert_eq!(catalogue_url(Some("   ")), None);
    }

    #[test]
    fn normalize_parses_openrouter_shape() {
        let body = serde_json::json!({
            "data": [
                {
                    "id": "anthropic/claude-opus-4",
                    "name": "Anthropic: Claude Opus 4",
                    "context_length": 200000,
                    "pricing": { "prompt": "0.000015", "completion": "0.000075" }
                },
                {
                    "id": "deepseek/deepseek-v4-pro",
                    "context_length": 0,
                    "pricing": { "prompt": 0.0000002 }
                },
                { "name": "no id — skipped" },
                { "id": "   " }
            ]
        });
        let got = normalize(&body);
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[0],
            ModelEntry {
                id: "anthropic/claude-opus-4".into(),
                name: Some("Anthropic: Claude Opus 4".into()),
                context_length: Some(200_000),
                prompt_price: Some("0.000015".into()),
                completion_price: Some("0.000075".into()),
            }
        );
        // context_length 0 dropped; number price coerced; no completion price.
        assert_eq!(
            got[1],
            ModelEntry {
                id: "deepseek/deepseek-v4-pro".into(),
                name: None,
                context_length: None,
                // Numbers are coerced via serde_json's own formatting
                // (real OpenRouter sends decimal strings; this is the
                // tolerant branch).
                prompt_price: Some("2e-7".into()),
                completion_price: None,
            }
        );
    }

    #[test]
    fn normalize_empty_on_missing_or_bad_data() {
        assert!(normalize(&serde_json::json!({})).is_empty());
        assert!(normalize(&serde_json::json!({ "data": "nope" })).is_empty());
        assert!(normalize(&serde_json::json!({ "data": [] })).is_empty());
    }
}
