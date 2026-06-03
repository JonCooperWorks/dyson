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
use crate::skill::mcp::elicitation;

/// Maximum size of an elicitation answer body.  Answers are small forms.
const MAX_ELICITATION_BODY: usize = 64 * 1024;

/// `GET /api/mcp/elicitations` — list the currently open prompts.
pub(super) async fn list() -> Resp {
    json_ok(&serde_json::json!({ "pending": elicitation::broker().list_pending().await }))
}

/// `POST /api/mcp/elicitations/:id` — answer an open prompt.  The body is
/// the MCP `ElicitResult` shape: `{ "action": "accept"|"decline"|"cancel",
/// "content"?: {...} }`.  Unknown / already-answered ids return 404.
pub(super) async fn respond(req: hyper::Request<hyper::body::Incoming>, id: &str) -> Resp {
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
    if elicitation::broker().resolve(id, body).await {
        json_ok(&serde_json::json!({ "ok": true }))
    } else {
        not_found()
    }
}
