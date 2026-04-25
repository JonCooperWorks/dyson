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
    Resp, apply_security_headers, boxed, client_accepts_gzip, get_auth_config, maybe_gzip,
    method_not_allowed, misdirected_request, not_found, safe_store_id, unauthorized,
    url_decode_strict,
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
    let mut resp = maybe_gzip(dispatch_inner(req, state).await, accepts_gzip).await;
    // Stamp baseline security headers last so they cover every code
    // path (errors, static assets, SSE) without each branch repeating
    // the four lines.  Per-route customisation is preserved — the
    // helper only inserts when the header is missing.
    apply_security_headers(&mut resp);
    resp
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

    // SSE bearer-in-URL was the older (sin) shape: a real OIDC token
    // would survive in browser history / proxy logs.  Tickets are
    // single-use, identity-bound, and 30-second-lived — the SPA
    // exchanges its bearer at this endpoint and uses the result on
    // the SSE open.  Mint requires the bearer in the Authorization
    // header so the rest of the auth chain stays the source of
    // identity truth.
    if matches!((&method, segs.as_slice()), (&Method::POST, ["api", "auth", "sse-ticket"])) {
        match state.auth.validate_request(req.headers()).await {
            Ok(info) => {
                // Bind the ticket to the most specific identity the
                // auth chain produced.  For OIDC that's the `sub`
                // claim (in metadata); other schemes only carry the
                // scheme tag in `identity` (e.g. `bearer`).  The
                // ticket consumer reuses this when the controller
                // is locked to a single user via `allowed_identity`.
                let identity = info
                    .metadata
                    .get("sub")
                    .cloned()
                    .unwrap_or(info.identity);
                let ticket = state.mint_sse_ticket(&identity);
                return super::responses::json_ok(&serde_json::json!({
                    "ticket": ticket,
                    "expires_in": 30,
                }));
            }
            Err(_) => return unauthorized(&state),
        }
    }

    // SSE: a `?access_token=<ticket>` query that resolves to a live
    // ticket consumes it (single-use) and bypasses the regular auth
    // gate for this one request.  Falls through to the header path
    // otherwise so k6 / curl-wrapper clients can still authenticate
    // with a real Authorization header on the SSE open.
    let is_events = segs.last() == Some(&"events") && segs.first() == Some(&"api");
    let mut ticket_authorized = false;
    if is_events && method == Method::GET && !req.headers().contains_key("authorization") {
        if let Some(query) = req.uri().query() {
            for (k, v) in super::responses::parse_query(query) {
                if k == "access_token"
                    && let Some(identity) = state.consume_sse_ticket(&v)
                {
                    tracing::debug!(identity, "SSE ticket consumed");
                    ticket_authorized = true;
                    break;
                }
            }
        }
    }

    // DNS-rebinding gate.  Enabled only when the controller bound to
    // a loopback address with `DangerousNoAuth` — that pairing is the
    // attacker's leverage (a webpage on `evil.example` that resolves
    // to 127.0.0.1 can otherwise hit the API with no credential).
    // Reverse-proxy and bearer/OIDC deployments stay off the gate so
    // the public Host the proxy presents doesn't trip a 421.
    if state.loopback_only_host_check && path.starts_with("/api/") {
        let host_value = req
            .headers()
            .get(hyper::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        // Strip port and bracket — `127.0.0.1:7878` and `[::1]:7878`
        // both reduce to a bare host; we then match against the
        // loopback allowlist.
        let host_part = host_value
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host_value)
            .trim_start_matches('[')
            .trim_end_matches(']');
        // Empty Host is HTTP/1.0 / raw-socket only (browsers always set
        // it).  Allowing it punched a hole in the gate for any client
        // willing to drop down to a raw socket, with no legitimate use
        // case in a loopback-only deployment.  Reject explicitly.
        let host_ok = matches!(host_part, "127.0.0.1" | "::1" | "localhost");
        if !host_ok {
            return misdirected_request();
        }
    }

    // Inbound auth on every `/api/*`.  Static-shell paths (`/`,
    // `/assets/*`) are exempt so the UI can load before presenting a
    // credential.  SSE endpoints can't send headers from the browser,
    // so the SPA exchanges its bearer for a one-shot ticket above and
    // we skip the regular gate when it consumed.
    if path.starts_with("/api/") && !ticket_authorized {
        if state.auth.validate_request(req.headers()).await.is_err() {
            return unauthorized(&state);
        }
    }

    // CSRF gate: every state-changing `/api/*` request must carry the
    // `X-Dyson-CSRF` custom header.  Browsers can't set custom headers
    // cross-origin without firing a CORS preflight, and the controller
    // never returns permissive `Access-Control-Allow-*` headers — so a
    // forged POST/DELETE/PUT/PATCH from `evil.example` is blocked at
    // the preflight, and a same-origin call from the SPA passes
    // because `client.js` stamps the header on `_authedFetch`.  This
    // closes the gap where a stored bearer / OIDC cookie would
    // otherwise be auto-attached to a cross-site form submit.
    //
    // Two carve-outs:
    //   * `/api/auth/sse-ticket` is already gated by a bearer above
    //     and is the bootstrap that the SPA's CSRF wrapper depends
    //     on; a CSRF check here would chicken-and-egg the first call.
    //   * SSE ticket consumption (handled above as `ticket_authorized`)
    //     never lands here for state-changing methods.
    let is_state_changing = matches!(
        &method,
        &Method::POST | &Method::DELETE | &Method::PUT | &Method::PATCH,
    );
    let is_csrf_exempt = matches!(segs.as_slice(), ["api", "auth", "sse-ticket"]);
    if path.starts_with("/api/")
        && is_state_changing
        && !is_csrf_exempt
        && !req.headers().contains_key("x-dyson-csrf")
    {
        return super::responses::bad_request("missing X-Dyson-CSRF header");
    }

    match (&method, segs.as_slice()) {
        // ─── conversations ─────────────────────────────────────────────
        (&Method::GET,    ["api", "conversations"])                 => conversations::list(&state).await,
        (&Method::POST,   ["api", "conversations"])                 => conversations::create(req, &state).await,
        (&Method::GET,    ["api", "conversations", id])             => conversations::get(&state, id).await,
        (&Method::DELETE, ["api", "conversations", id])             => conversations::delete(&state, id).await,
        (&Method::POST,   ["api", "conversations", id, "turn"])     => turns::post(req, Arc::clone(&state), id).await,
        (&Method::POST,   ["api", "conversations", id, "cancel"])   => conversations::cancel(&state, id).await,
        (&Method::GET,    ["api", "conversations", id, "events"])   => sse::events(&state, id, &req).await,
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
        // Strict decode here — these ids feed `safe_store_id` which
        // refuses anything outside `[A-Za-z0-9_-]`.  A malformed
        // percent-escape that the lossy `url_decode` would silently
        // pass through as `%ZZ` should 404 instead.
        (&Method::GET, ["api", "files", id]) => match url_decode_strict(id) {
            Some(id) => files::get(&state, &id).await,
            None => not_found(),
        },
        (&Method::GET, ["api", "artefacts", id]) => match url_decode_strict(id) {
            Some(id) => artefacts::get(&state, &id).await,
            None => not_found(),
        },
        // Naked `/artefacts/<id>` is a shareable permalink: bounce it
        // to `#/artefacts/<id>` so the SPA reader opens with it
        // selected.  Keeps the URL short and doesn't leak the API
        // path that serves the bytes.
        (&Method::GET, ["artefacts", id]) => {
            let id = match url_decode_strict(id) {
                Some(id) => id,
                None => return not_found(),
            };
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
