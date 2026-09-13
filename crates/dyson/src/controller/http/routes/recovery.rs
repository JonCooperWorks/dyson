//! Operator inspection and explicit resolution of unknown tool outcomes.
use super::super::responses::{
    Resp, bad_request, json_ok, not_found, read_json_capped, safe_store_id,
};
use super::super::state::HttpState;
use super::super::wire::MAX_SMALL_BODY;
use hyper::Request;

pub(super) async fn get(state: &HttpState, id: &str) -> Resp {
    if !safe_store_id(id) {
        return not_found();
    }
    let Some(store) = &state.history else {
        return bad_request("execution journal not configured");
    };
    match store.load_run_events(id) {
        Ok(events) => json_ok(&crate::agent::protocol::unresolved_tool_outcomes(&events)),
        Err(_) => bad_request("cannot read execution journal"),
    }
}

#[derive(serde::Deserialize)]
struct Resolution {
    run_id: crate::agent::protocol::RunId,
    tool_use_id: String,
    resolution: String,
}

pub(super) async fn post(req: Request<hyper::body::Incoming>, state: &HttpState, id: &str) -> Resp {
    if !safe_store_id(id) {
        return not_found();
    }
    let body: Resolution = match read_json_capped(req, MAX_SMALL_BODY).await {
        Ok(body) => body,
        Err(error) => return bad_request(&error),
    };
    let Some(chat) = state.chats.lock().await.get(id).cloned() else {
        return not_found();
    };
    let Ok(mut guard) = chat.agent.try_lock() else {
        return bad_request("stop the active turn before reconciling");
    };
    let Some(agent) = guard.as_mut() else {
        return bad_request("load this conversation with a read-only turn before reconciling");
    };
    match agent.reconcile_tool_outcome(&body.run_id, &body.tool_use_id, &body.resolution) {
        Ok(()) => json_ok(&serde_json::json!({"ok":true})),
        Err(error) => bad_request(&error.sanitized_message()),
    }
}
