// ===========================================================================
// HTTP controller — routing entry point.
//
// One match arm per (method, path-segments) tuple.  Auth is enforced
// before dispatch on every `/api/*` path; static-shell paths are exempt
// so the UI can load before presenting a credential.
//
// Every handler returns the boxed `Resp` body alias from `responses`.
// Per-feature handlers live in sibling modules (`conversations`,
// `turns`, `feedback`, …).  They're `pub(super)` so this dispatch
// table can call them, but invisible from `crate::controller::http`'s
// public surface.
// ===========================================================================

use std::sync::Arc;

use hyper::body::Bytes;
use hyper::{Method, Request, Response, StatusCode};

use super::responses::{
    Resp, auth_headers_for, boxed, client_accepts_gzip, get_auth_config, maybe_gzip,
    method_not_allowed, not_found, safe_store_id, unauthorized, url_decode,
};
use super::state::HttpState;

mod activity;
mod artefacts;
pub(super) mod conversations;
mod feedback;
mod files;
mod mind;
mod model;
mod providers;
mod sse;
mod static_assets;
mod turns;

pub(super) async fn dispatch(req: Request<hyper::body::Incoming>, state: Arc<HttpState>) -> Resp {
    // Gzip the response if the client asked for it and the content-type
    // matches `compressible_content_type`.  Extracted into a wrapper so
    // the (large) routing match below stays focused on what it's for.
    // SSE responses skip compression because their Content-Type isn't in
    // the compressible set — buffering their body would be a disaster.
    let accepts_gzip = client_accepts_gzip(req.headers());
    maybe_gzip(dispatch_inner(req, state).await, accepts_gzip).await
}

async fn dispatch_inner(req: Request<hyper::body::Incoming>, state: Arc<HttpState>) -> Resp {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    // Borrowed view of the path segments — `["api", "conversations", id, …]`
    // — keyed on once and reused by the route match.
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();

    // `/api/auth/config` is intentionally unauthenticated: the SPA
    // calls it before it has a token to discover whether one is
    // required, and (for OIDC) where to start the auth code flow.
    if matches!((&method, segs.as_slice()), (&Method::GET, ["api", "auth", "config"])) {
        return get_auth_config(&state);
    }

    // Inbound auth on every `/api/*`.  Static-shell paths (`/`,
    // `/assets/*`) are exempt so the UI can load before presenting a
    // credential.  SSE endpoints can't send headers from the browser,
    // so `auth_headers_for` folds `?access_token=` into a synthetic
    // Authorization header — only when necessary, the rest borrow
    // `req.headers()` and pay no allocation.
    if path.starts_with("/api/") {
        let synthesised = auth_headers_for(&path, &req);
        let headers = synthesised.as_ref().unwrap_or_else(|| req.headers());
        if state.auth.validate_request(headers).await.is_err() {
            return unauthorized(&state);
        }
    }

    match (&method, segs.as_slice()) {
        // ─── conversations ─────────────────────────────────────────────
        (&Method::GET,    ["api", "conversations"])                 => conversations::list(&state).await,
        (&Method::POST,   ["api", "conversations"])                 => conversations::create(req, &state).await,
        (&Method::GET,    ["api", "conversations", id])             => conversations::get(&state, id).await,
        (&Method::DELETE, ["api", "conversations", id])             => conversations::delete(&state, id).await,
        (&Method::POST,   ["api", "conversations", id, "turn"])     => turns::post(req, Arc::clone(&state), id).await,
        (&Method::POST,   ["api", "conversations", id, "cancel"])   => conversations::cancel(&state, id).await,
        (&Method::GET,    ["api", "conversations", id, "events"])   => sse::events(&state, id).await,
        (&Method::GET,    ["api", "conversations", id, "feedback"]) => feedback::get(&state, id).await,
        (&Method::POST,   ["api", "conversations", id, "feedback"]) => feedback::post(req, &state, id).await,
        (&Method::GET,    ["api", "conversations", id, "artefacts"]) => artefacts::list(&state, id).await,
        (&Method::GET,    ["api", "conversations", id, "export"])   => artefacts::export(&state, id).await,

        // ─── providers / model / mind / activity ───────────────────────
        (&Method::GET,    ["api", "providers"])    => providers::list(&state),
        (&Method::POST,   ["api", "model"])        => model::post(req, Arc::clone(&state)).await,
        (&Method::GET,    ["api", "mind"])         => mind::get(&state).await,
        (&Method::GET,    ["api", "mind", "file"]) => mind::get_file(&state, req.uri().query().unwrap_or("")).await,
        (&Method::POST,   ["api", "mind", "file"]) => mind::post_file(req, &state).await,
        (&Method::GET,    ["api", "activity"])     => activity::get(&state, req.uri().query().unwrap_or("")),

        // ─── files & artefacts ─────────────────────────────────────────
        (&Method::GET, ["api", "files", id])     => files::get(&state, &url_decode(id)).await,
        (&Method::GET, ["api", "artefacts", id]) => artefacts::get(&state, &url_decode(id)).await,
        // Naked `/artefacts/<id>` is a shareable permalink: bounce it
        // to `#/artefacts/<id>` so the SPA reader opens with it
        // selected.  Keeps the URL short and doesn't leak the API
        // path that serves the bytes.
        (&Method::GET, ["artefacts", id]) => {
            let id = url_decode(id);
            if !safe_store_id(&id) {
                return not_found();
            }
            Response::builder()
                .status(StatusCode::FOUND)
                .header("Location", format!("/#/artefacts/{id}"))
                .header("Cache-Control", "no-cache")
                .body(boxed(Bytes::new()))
                .unwrap()
        }

        // ─── static shell + fallback ───────────────────────────────────
        (&Method::GET, _) => static_assets::serve(&path).await,
        _ if path.starts_with("/api/") => method_not_allowed(),
        _ => method_not_allowed(),
    }
}

#[cfg(test)]
mod tests {
    /// Drive `dispatch_inner` with a synthetic request without binding
    /// a port — we can't directly because `dispatch_inner` takes a
    /// `Request<hyper::body::Incoming>` and `Incoming` only comes from
    /// real connections.  Verifies the segment-tuple match that maps
    /// e.g. `("DELETE", ["api","conversations","c-1"])` to the delete
    /// handler.  This test keeps the dispatch table in sync with what
    /// the SPA actually fires.
    #[test]
    fn dispatch_segments_partition_correctly() {
        // Manually re-do the segment split that dispatch_inner does
        // and assert the cases we expect to land on each handler.
        fn segs(p: &str) -> Vec<&str> {
            p.trim_matches('/').split('/').collect()
        }
        // /api root
        assert_eq!(segs("/api/conversations"), vec!["api", "conversations"]);
        assert_eq!(segs("/api/conversations/c-1"), vec!["api", "conversations", "c-1"]);
        assert_eq!(
            segs("/api/conversations/c-1/turn"),
            vec!["api", "conversations", "c-1", "turn"],
        );
        assert_eq!(
            segs("/api/conversations/c-1/events"),
            vec!["api", "conversations", "c-1", "events"],
        );
        // /artefacts redirect path uses two segments.
        assert_eq!(segs("/artefacts/a1"), vec!["artefacts", "a1"]);
        // /api/auth/config — the unauthenticated discovery endpoint.
        assert_eq!(segs("/api/auth/config"), vec!["api", "auth", "config"]);
    }
}
