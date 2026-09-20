//! Run controls use the same authenticated dispatch path as ordinary turns.
use super::super::{
    responses::{Resp, bad_request, json_ok, not_found, read_json_capped, safe_store_id},
    state::HttpState,
    wire::MAX_SMALL_BODY,
};
use crate::agent::{
    continuation::{HumanAnswer, RunCheckpoint, RunState},
    protocol::RunId,
};
use hyper::Request;
use std::sync::Arc;

#[derive(serde::Deserialize)]
pub(super) struct ResumeRequest {
    pub run_id: RunId,
    #[serde(default)]
    pub answer: Option<HumanAnswer>,
}

pub(super) async fn get(state: &HttpState, id: &str) -> Resp {
    if !safe_store_id(id) {
        return not_found();
    }
    let Some(store) = &state.history else {
        return bad_request("durable history is not configured");
    };
    match RunCheckpoint::load(store.as_ref(), id) {
        Ok(Some(record)) => json_ok(&record.view()),
        Ok(None) => not_found(),
        Err(e) => bad_request(&e.sanitized_message()),
    }
}

pub(super) async fn pause(
    req: Request<hyper::body::Incoming>,
    state: &HttpState,
    id: &str,
) -> Resp {
    if !safe_store_id(id) {
        return not_found();
    }
    let body: ResumeRequest = match read_json_capped(req, MAX_SMALL_BODY).await {
        Ok(body) => body,
        Err(e) => return bad_request(&e),
    };
    let Some(store) = &state.history else {
        return bad_request("durable history is not configured");
    };
    let record = match RunCheckpoint::load(store.as_ref(), id) {
        Ok(Some(record)) if record.cursor.run_id == body.run_id => record,
        Ok(_) => return not_found(),
        Err(e) => return bad_request(&e.sanitized_message()),
    };
    if matches!(record.cursor.state, RunState::Finished { .. }) {
        return bad_request("run has finished");
    }
    match store.save_harness_record(
        id,
        "pause-request",
        &serde_json::json!({"run_id":body.run_id}),
    ) {
        Ok(()) => json_ok(&serde_json::json!({"pause_requested":true,"run_id":body.run_id})),
        Err(e) => bad_request(&e.sanitized_message()),
    }
}

pub(super) async fn resume(
    req: Request<hyper::body::Incoming>,
    state: Arc<HttpState>,
    id: &str,
) -> Resp {
    if !safe_store_id(id) {
        return not_found();
    }
    let body: ResumeRequest = match read_json_capped(req, MAX_SMALL_BODY).await {
        Ok(body) => body,
        Err(e) => return bad_request(&e),
    };
    super::turns::resume(state, id, body).await
}
