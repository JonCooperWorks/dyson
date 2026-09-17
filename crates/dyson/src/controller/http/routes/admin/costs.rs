//! Reconcile historical message costs with Swarm accounting.
use super::{HttpState, Resp, bad_request, json_ok, json_status, read_json_capped};
use hyper::{Request, StatusCode};
use serde::Deserialize;

const MAX_COST_BACKFILL_BODY: usize = 1024;

#[derive(Debug, Default, Deserialize)]
pub(super) struct CostBackfillBody {
    #[serde(default)]
    dry_run: bool,
}

/// Fleet operation hook used by swarmctl after Dyson rollouts.
///
/// This route is still protected by the controller's normal `/api/*` bearer
/// gate and CSRF header. It intentionally does not require the configure
/// secret because swarmctl discovers live instances from Swarm's DB and calls
/// them with their per-instance bearer token.
pub(in super::super) async fn post_cost_backfill(
    req: Request<hyper::body::Incoming>,
    state: &HttpState,
) -> Resp {
    let body: CostBackfillBody = match read_json_capped(req, MAX_COST_BACKFILL_BODY).await {
        Ok(body) => body,
        Err(err) => return bad_request(&err),
    };
    let Some(history) = state.history.as_ref() else {
        return bad_request("chat history backend is not configured");
    };
    let Some(costs) = crate::swarm_cost::config_snapshot_or_env() else {
        return json_status(
            StatusCode::SERVICE_UNAVAILABLE,
            &serde_json::json!({
                "ok": false,
                "error": "Swarm cost lookup is not configured"
            }),
        );
    };
    match crate::message_cost_backfill::backfill_history(
        history.as_ref(),
        &costs,
        crate::message_cost_backfill::CostBackfillOptions {
            dry_run: body.dry_run,
        },
    )
    .await
    {
        Ok(report) => json_ok(&serde_json::json!({
            "ok": true,
            "dry_run": body.dry_run,
            "messages_scanned": report.messages_scanned,
            "messages_linked": report.messages_linked,
            "messages_priced": report.messages_priced,
            "messages_skipped": report.messages_skipped,
            "skip_reasons": report.skip_reasons,
        })),
        Err(err) => {
            tracing::warn!(error = %err, "cost backfill failed");
            json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({
                    "ok": false,
                    "error": "cost backfill failed"
                }),
            )
        }
    }
}
