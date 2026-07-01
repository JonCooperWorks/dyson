//! Backfill assistant-message display costs from Swarm audit rows.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::chat_history::ChatHistory;
use crate::message::{MessageCostMetadata, Role};
use crate::swarm_cost::{CostLookupConfig, fetch_cost_call, metadata_from_cost_call};

#[derive(Debug, Default, Clone, Serialize)]
pub struct CostBackfillReport {
    pub messages_scanned: u64,
    pub messages_linked: u64,
    pub messages_priced: u64,
    pub messages_skipped: u64,
    pub skip_reasons: BTreeMap<String, u64>,
}

impl CostBackfillReport {
    fn skip(&mut self, reason: &'static str) {
        self.messages_skipped += 1;
        *self.skip_reasons.entry(reason.to_string()).or_insert(0) += 1;
    }

    fn add(&mut self, other: CostBackfillReport) {
        self.messages_scanned += other.messages_scanned;
        self.messages_linked += other.messages_linked;
        self.messages_priced += other.messages_priced;
        self.messages_skipped += other.messages_skipped;
        for (reason, count) in other.skip_reasons {
            *self.skip_reasons.entry(reason).or_insert(0) += count;
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CostBackfillOptions {
    pub dry_run: bool,
}

pub async fn backfill_history(
    history: &dyn ChatHistory,
    config: &CostLookupConfig,
    options: CostBackfillOptions,
) -> crate::Result<CostBackfillReport> {
    let client = crate::http::client();
    let mut report = CostBackfillReport::default();

    for chat_id in history.list()? {
        let chat_report = backfill_chat(history, client, config, options, &chat_id).await?;
        report.add(chat_report);
    }

    Ok(report)
}

async fn backfill_chat(
    history: &dyn ChatHistory,
    client: &reqwest::Client,
    config: &CostLookupConfig,
    options: CostBackfillOptions,
    chat_id: &str,
) -> crate::Result<CostBackfillReport> {
    let mut report = CostBackfillReport::default();
    let mut messages = history.load(chat_id)?;
    let mut changed = false;

    for msg in messages.iter_mut() {
        report.messages_scanned += 1;
        if msg.role != Role::Assistant {
            report.skip("not_assistant");
            continue;
        }
        let Some(audit_id) = msg.cost.as_ref().and_then(|c| c.swarm_llm_audit_id) else {
            // Current transcript files do not carry reliable per-message
            // timestamps, so no-audit-id correlation is intentionally skipped.
            report.skip("no_audit_id");
            continue;
        };
        report.messages_linked += 1;
        let call = match fetch_cost_call(client, config, audit_id).await {
            Ok(Some(call)) => call,
            Ok(None) => {
                report.skip("not_found");
                continue;
            }
            Err(err) => {
                tracing::warn!(chat_id, audit_id, error = %err, "Swarm cost lookup failed");
                report.skip("lookup_failed");
                continue;
            }
        };
        let Some(finalized) = metadata_from_cost_call(call, None) else {
            report.skip("unpriced");
            continue;
        };
        merge_cost(
            msg.cost.get_or_insert(MessageCostMetadata {
                swarm_llm_audit_id: Some(audit_id),
                display_cost_usd: None,
                cost_source: None,
                cost_finalized_at: None,
                provider: None,
                model: None,
                input_tokens: None,
                output_tokens: None,
                key_source: None,
            }),
            finalized,
        );
        report.messages_priced += 1;
        changed = true;
    }

    if changed && !options.dry_run {
        history.save(chat_id, &messages)?;
    }

    Ok(report)
}

pub fn merge_cost(target: &mut MessageCostMetadata, source: MessageCostMetadata) {
    target.swarm_llm_audit_id = source.swarm_llm_audit_id.or(target.swarm_llm_audit_id);
    target.display_cost_usd = source.display_cost_usd.or(target.display_cost_usd);
    target.cost_source = source.cost_source.or_else(|| target.cost_source.clone());
    target.cost_finalized_at = source.cost_finalized_at.or(target.cost_finalized_at);
    target.provider = source.provider.or_else(|| target.provider.clone());
    target.model = source.model.or_else(|| target.model.clone());
    target.input_tokens = source.input_tokens.or(target.input_tokens);
    target.output_tokens = source.output_tokens.or(target.output_tokens);
    target.key_source = source.key_source.or_else(|| target.key_source.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_history::DiskChatHistory;
    use crate::message::Message;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn merge_cost_preserves_existing_when_source_lacks_optional_values() {
        let mut target = MessageCostMetadata {
            swarm_llm_audit_id: Some(1),
            display_cost_usd: None,
            cost_source: Some("provider_reported".into()),
            cost_finalized_at: None,
            provider: Some("openrouter".into()),
            model: Some("model-a".into()),
            input_tokens: Some(1),
            output_tokens: None,
            key_source: None,
        };
        let source = MessageCostMetadata {
            swarm_llm_audit_id: Some(1),
            display_cost_usd: Some(0.0031),
            cost_source: None,
            cost_finalized_at: Some(5),
            provider: None,
            model: None,
            input_tokens: None,
            output_tokens: Some(2),
            key_source: Some("platform".into()),
        };
        merge_cost(&mut target, source);
        assert_eq!(target.display_cost_usd, Some(0.0031));
        assert_eq!(target.cost_source.as_deref(), Some("provider_reported"));
        assert_eq!(target.provider.as_deref(), Some("openrouter"));
        assert_eq!(target.output_tokens, Some(2));
        assert_eq!(target.key_source.as_deref(), Some("platform"));
    }

    #[tokio::test]
    async fn backfill_prices_messages_with_existing_swarm_audit_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/costs/calls/42"))
            .and(header("authorization", "Bearer user-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "audit_id": 42,
                "provider": "openrouter",
                "instance_id": "inst-1",
                "model": "anthropic/claude",
                "key_source": "platform",
                "status_code": 200,
                "occurred_at": 1,
                "input_tokens": 1200,
                "output_tokens": 340,
                "total_tokens": 1540,
                "cost_usd": 0.0031,
                "cost_source": "provider_reported"
            })))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let history = DiskChatHistory::new(dir.path().to_path_buf()).unwrap();
        history
            .save(
                "c-1",
                &[
                    Message::user("hi"),
                    Message {
                        role: Role::Assistant,
                        content: vec![crate::message::ContentBlock::Text {
                            text: "done".into(),
                        }],
                        context_summary: false,
                        cost: Some(MessageCostMetadata {
                            swarm_llm_audit_id: Some(42),
                            display_cost_usd: None,
                            cost_source: None,
                            cost_finalized_at: None,
                            provider: None,
                            model: None,
                            input_tokens: None,
                            output_tokens: None,
                            key_source: None,
                        }),
                    },
                ],
            )
            .unwrap();
        let config = CostLookupConfig::public_api(&server.uri(), Some("user-token")).unwrap();

        let report = backfill_history(&history, &config, CostBackfillOptions { dry_run: false })
            .await
            .unwrap();

        assert_eq!(report.messages_linked, 1);
        assert_eq!(report.messages_priced, 1);
        let loaded = history.load("c-1").unwrap();
        let cost = loaded[1].cost.as_ref().unwrap();
        assert_eq!(cost.display_cost_usd, Some(0.0031));
        assert_eq!(cost.provider.as_deref(), Some("openrouter"));
        assert_eq!(cost.model.as_deref(), Some("anthropic/claude"));
        assert_eq!(cost.input_tokens, Some(1200));
        assert_eq!(cost.output_tokens, Some(340));
    }
}
