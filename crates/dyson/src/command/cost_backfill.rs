use std::collections::BTreeMap;
use std::path::PathBuf;

use dyson::chat_history::create_chat_history;
use dyson::message::{MessageCostMetadata, Role};

#[derive(Debug, Default)]
struct Report {
    scanned: u64,
    linked: u64,
    priced: u64,
    skipped: u64,
    reasons: BTreeMap<&'static str, u64>,
}

#[derive(Debug, serde::Deserialize)]
struct SwarmCostCall {
    audit_id: i64,
    provider: String,
    model: Option<String>,
    key_source: String,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cost_usd: Option<f64>,
    cost_source: String,
}

pub async fn run(
    config: Option<PathBuf>,
    swarm_url: String,
    bearer: Option<String>,
    dry_run: bool,
) -> dyson::error::Result<()> {
    let config_path = super::resolve_config_path(config);
    let settings = dyson::config::loader::load_settings(config_path.as_deref())?;
    let history = create_chat_history(&settings.chat_history)?;
    let client = reqwest::Client::builder().build()?;
    let base = swarm_url.trim_end_matches('/').to_string();
    let mut report = Report::default();

    for chat_id in history.list()? {
        let mut messages = history.load(&chat_id)?;
        let mut changed = false;
        for msg in messages.iter_mut() {
            report.scanned += 1;
            if msg.role != Role::Assistant {
                skip(&mut report, "not_assistant");
                continue;
            }
            let Some(audit_id) = msg.cost.as_ref().and_then(|c| c.swarm_llm_audit_id) else {
                skip(&mut report, "no_audit_id");
                continue;
            };
            report.linked += 1;
            let call = match fetch_cost_call(&client, &base, bearer.as_deref(), audit_id).await {
                Ok(Some(call)) => call,
                Ok(None) => {
                    skip(&mut report, "not_found");
                    continue;
                }
                Err(err) => {
                    tracing::warn!(chat_id = %chat_id, audit_id, error = %err, "Swarm cost lookup failed");
                    skip(&mut report, "lookup_failed");
                    continue;
                }
            };
            if call.cost_usd.is_none() {
                skip(&mut report, "unpriced");
                continue;
            }
            msg.cost = Some(MessageCostMetadata {
                swarm_llm_audit_id: Some(call.audit_id),
                display_cost_usd: call.cost_usd,
                cost_source: Some(call.cost_source),
                cost_finalized_at: None,
                provider: Some(call.provider),
                model: call.model,
                input_tokens: call.input_tokens,
                output_tokens: call.output_tokens,
                key_source: Some(call.key_source),
            });
            report.priced += 1;
            changed = true;
        }
        if changed && !dry_run {
            history.save(&chat_id, &messages)?;
        }
    }

    println!("messages scanned: {}", report.scanned);
    println!("messages linked: {}", report.linked);
    println!("messages priced: {}", report.priced);
    println!("messages skipped: {}", report.skipped);
    for (reason, count) in report.reasons {
        println!("skip {reason}: {count}");
    }
    Ok(())
}

fn skip(report: &mut Report, reason: &'static str) {
    report.skipped += 1;
    *report.reasons.entry(reason).or_insert(0) += 1;
}

async fn fetch_cost_call(
    client: &reqwest::Client,
    base: &str,
    bearer: Option<&str>,
    audit_id: i64,
) -> dyson::error::Result<Option<SwarmCostCall>> {
    let url = format!("{base}/v1/costs/calls/{audit_id}");
    let mut req = client.get(url).header("accept", "application/json");
    if let Some(token) = bearer.filter(|s| !s.is_empty()) {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let resp = resp.error_for_status()?;
    Ok(Some(resp.json::<SwarmCostCall>().await?))
}
