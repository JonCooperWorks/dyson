// ===========================================================================
// /api/activity[?chat=<id>] — running / recent subagent activity for
// the Activity tab.  Reads from the disk-backed `ActivityRegistry`.
// ===========================================================================

use super::super::responses::{Resp, json_ok, parse_query};
use super::super::state::HttpState;

/// Per-lane activity surfaced in the web UI's Activity tab.
///
/// Reads from `HttpState.activity` (the `ActivityRegistry`, disk-backed
/// per chat).  Returns a JSON payload the frontend's `ActivityView`
/// consumes directly:
///
/// ```json
/// { "lanes": [
///     { "lane": "subagent", "name": "security_engineer",
///       "note": "Review crate for OWASP...", "status": "running",
///       "last": "1714053234", "chat_id": "c-0023" },
///     ...
/// ] }
/// ```
///
/// Query params:
/// - `?chat=<id>` — filter to one chat (single-chat view)
/// - default     — all chats, newest-first
///
/// Other lanes (`loop` / `dream` / `swarm`) don't feed this registry
/// yet; the frontend already renders them from separate data sources.
/// Keeping the response schema uniform means extending the registry
/// later is additive, not a rewrite.
pub(super) fn get(state: &HttpState, query: &str) -> Resp {
    let chat_filter = parse_query(query)
        .into_iter()
        .find(|(k, _)| k == "chat")
        .map(|(_, v)| v);

    let entries = match chat_filter.as_deref() {
        Some(cid) => state.activity.snapshot_chat(cid),
        None => state.activity.snapshot_all(),
    };

    let lanes: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|e| {
            let status = match e.status {
                crate::controller::ActivityStatus::Running => "running",
                crate::controller::ActivityStatus::Ok => "ok",
                crate::controller::ActivityStatus::Err => "err",
            };
            let last = e
                .finished_at
                .map(|t| t.to_string())
                .unwrap_or_else(|| e.started_at.to_string());
            serde_json::json!({
                "lane": e.lane,
                "name": e.name,
                "note": e.note,
                "status": status,
                "last": last,
                "chat_id": e.chat_id,
                "started_at": e.started_at,
                "finished_at": e.finished_at,
            })
        })
        .collect();

    json_ok(&serde_json::json!({ "lanes": lanes }))
}
