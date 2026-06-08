// ===========================================================================
// /api/audit — per-request LLM audit rows for the Audit tab.
//
// Swarm is the source of truth: it records every proxied LLM call with
// tokens, cost, latency, and (locally-measured) streaming throughput.
// This route forwards the request to swarm's per-instance internal
// endpoint using the instance's own proxy token, then hands the rows to
// the web UI verbatim.  Standalone dyson (no swarm wired) returns an
// empty list with `source: "unavailable"` so the tab can explain itself.
// ===========================================================================

use super::super::responses::{Resp, json_ok, parse_query};
use crate::swarm_cost::{config_snapshot_or_env, fetch_audit_calls};

/// Query params we forward to swarm.  Everything else is dropped so the
/// agent can't be used to smuggle arbitrary query strings upstream.
const FORWARDED_PARAMS: [&str; 4] = ["range", "since", "until", "limit"];

pub(super) async fn get(query: &str) -> Resp {
    let forwarded = sanitize_query(query);

    let Some(config) = config_snapshot_or_env() else {
        return json_ok(&serde_json::json!({
            "requests": [],
            "source": "unavailable",
        }));
    };

    match fetch_audit_calls(crate::http::client(), &config, &forwarded).await {
        Ok(requests) => json_ok(&serde_json::json!({
            "requests": requests,
            "source": "swarm",
        })),
        Err(e) => {
            tracing::warn!(error = %e, "audit list fetch from swarm failed");
            json_ok(&serde_json::json!({
                "requests": [],
                "source": "error",
            }))
        }
    }
}

fn sanitize_query(raw: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for (k, v) in parse_query(raw) {
        if FORWARDED_PARAMS.contains(&k.as_str()) {
            out.push(format!("{}={}", urlencode(&k), urlencode(&v)));
        }
    }
    out.join("&")
}

fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
