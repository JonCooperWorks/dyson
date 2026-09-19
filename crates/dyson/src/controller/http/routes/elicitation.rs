// ===========================================================================
// /api/mcp/elicitations — bridge MCP server-originated elicitation prompts
// to the human at the web UI.
//
//   GET  /api/mcp/elicitations        → open prompts the UI should show
//   POST /api/mcp/elicitations/:id     → answer one ({action, content?})
//
// Both inherit the controller's central `/api/*` auth + CSRF gate; the SPA
// short-polls the GET and submits the POST.  The backing store is the
// process-global elicitation broker in `skill::mcp::elicitation`.
// ===========================================================================

use super::super::responses::{Resp, bad_request, json_ok, not_found, read_json_capped};
use super::super::state::HttpState;
use crate::agent::continuation::{HumanAnswer, RunCheckpoint};
use crate::skill::mcp::elicitation;
use std::sync::Arc;

/// Maximum size of an elicitation answer body.  Answers are small forms.
const MAX_ELICITATION_BODY: usize = 64 * 1024;

/// `GET /api/mcp/elicitations` — list the currently open prompts.
pub(super) async fn list(state: &HttpState) -> Resp {
    let mut pending = elicitation::broker().list_pending().await;
    match durable_pending(state) {
        Ok(durable) => pending.extend(durable),
        Err(error) => return bad_request(&error.sanitized_message()),
    }
    json_ok(&serde_json::json!({ "pending": pending }))
}

fn durable_pending(state: &HttpState) -> crate::Result<Vec<serde_json::Value>> {
    let mut pending = Vec::new();
    if let Some(store) = &state.history {
        for chat in store.list_harness_chats()? {
            if let Some(record) = RunCheckpoint::load(store.as_ref(), &chat)? {
                if let Some(request) = record.pending_input(&chat) {
                    pending.push(request);
                }
            }
        }
    }
    pending.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Ok(pending)
}

/// `POST /api/mcp/elicitations/:id` — answer an open prompt.  The body is
/// the MCP `ElicitResult` shape: `{ "action": "accept"|"decline"|"cancel",
/// "content"?: {...} }`.  Unknown / already-answered ids return 404.
pub(super) async fn respond(
    req: hyper::Request<hyper::body::Incoming>,
    state: Arc<HttpState>,
    id: &str,
) -> Resp {
    let body: serde_json::Value = match read_json_capped(req, MAX_ELICITATION_BODY).await {
        Ok(v) => v,
        Err(msg) => return bad_request(&msg),
    };
    // Require a recognised action so we never forward a malformed result
    // to the waiting MCP server.
    let action = body.get("action").and_then(|a| a.as_str()).unwrap_or("");
    if !matches!(action, "accept" | "decline" | "cancel") {
        return bad_request("action must be accept, decline, or cancel");
    }
    if id.starts_with("durable-") {
        let pending = match durable_pending(&state) {
            Ok(p) => p,
            Err(e) => return bad_request(&e.sanitized_message()),
        };
        let Some(request) = pending.iter().find(|r| r["id"].as_str() == Some(id)) else {
            return not_found();
        };
        let answer: HumanAnswer = match serde_json::from_value(serde_json::json!({
            "request_id":request["request_id"],"action":action,"content":body.get("content").cloned().unwrap_or(serde_json::Value::Null)
        })) {
            Ok(a) => a,
            Err(e) => return bad_request(&e.to_string()),
        };
        let resume = super::runs::ResumeRequest {
            run_id: serde_json::from_value(request["run_id"].clone()).expect("serialized RunId"),
            answer: Some(answer),
        };
        return super::turns::resume(
            state,
            request["conversation_id"]
                .as_str()
                .expect("serialized chat id"),
            resume,
        )
        .await;
    }
    if elicitation::broker().resolve(id, body).await {
        json_ok(&serde_json::json!({ "ok": true }))
    } else {
        not_found()
    }
}
