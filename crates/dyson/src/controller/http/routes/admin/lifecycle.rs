//! Inspect or quiesce active turns during a fleet operation.
use super::{HttpState, Resp, authorize_configure, json_ok, json_status};
use hyper::{Request, StatusCode};

pub(in super::super) async fn get_idle(
    req: Request<hyper::body::Incoming>,
    state: &HttpState,
) -> Resp {
    if let Some(resp) = authorize_configure(req.headers(), state).await {
        return resp;
    }
    let in_flight = state.in_flight_chats().await;
    json_ok(&serde_json::json!({
        "ok": true,
        "idle": in_flight == 0,
        "in_flight_chats": in_flight,
        "quiesced": state.is_quiesced(),
    }))
}

pub(in super::super) async fn post_quiesce(
    req: Request<hyper::body::Incoming>,
    state: &HttpState,
) -> Resp {
    if let Some(resp) = authorize_configure(req.headers(), state).await {
        return resp;
    }
    let in_flight = state.try_quiesce().await;
    if in_flight != 0 {
        return json_status(
            StatusCode::CONFLICT,
            &serde_json::json!({
                "ok": false,
                "idle": false,
                "in_flight_chats": in_flight,
                "quiesced": false,
            }),
        );
    }
    json_ok(&serde_json::json!({
        "ok": true,
        "idle": true,
        "in_flight_chats": 0,
        "quiesced": true,
    }))
}

pub(in super::super) async fn post_unquiesce(
    req: Request<hyper::body::Incoming>,
    state: &HttpState,
) -> Resp {
    if let Some(resp) = authorize_configure(req.headers(), state).await {
        return resp;
    }
    state.unquiesce();
    json_ok(&serde_json::json!({
        "ok": true,
        "quiesced": false,
    }))
}
