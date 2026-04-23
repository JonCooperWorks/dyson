//! Integration tests for the HTTP controller — bind a real socket on
//! `127.0.0.1:0`, hit it with a hyper client, assert the wire contract
//! the web UI's `bridge.js` depends on.
//!
//! These complement the pure-helper unit tests in
//! `controller/http/mod.rs` (`#[cfg(test)] mod tests`) — those cover
//! emoji mapping, embedded asset table integrity, and the JS-side
//! regressions for ⌘4/⌘5 grey-screen and chats-open-at-top.

use std::sync::Arc;
use std::time::Duration;

use dyson::auth::{Auth, BearerTokenAuth, Credential, DangerousNoAuth};
use dyson::chat_history::{ChatHistory, DiskChatHistory};
use dyson::config::{ChatHistoryConfig, LlmProvider, ProviderConfig, Settings};
use dyson::controller::ClientRegistry;
use dyson::controller::http::{HttpState, test_helpers};
use dyson::feedback::FeedbackStore;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// Test rig
// ---------------------------------------------------------------------------

struct Rig {
    base: String,
    state: Arc<HttpState>,
    chat_dir: tempfile::TempDir,
    workspace_dir: tempfile::TempDir,
    _handle: JoinHandle<dyson::error::Result<()>>,
}

async fn rig() -> Rig {
    rig_with_auth(Arc::new(DangerousNoAuth)).await
}

async fn rig_with_auth(auth: Arc<dyn Auth>) -> Rig {
    let chat_dir = tempfile::tempdir().expect("chat tempdir");
    let workspace_dir = tempfile::tempdir().expect("workspace tempdir");

    let mut providers = std::collections::HashMap::new();
    providers.insert(
        "default".to_string(),
        ProviderConfig {
            provider_type: LlmProvider::OpenRouter,
            api_key: Credential::new("sk-test".into()),
            base_url: None,
            models: vec![
                "qwen/qwen3.6-plus".to_string(),
                "minimax/minimax-m2.5".to_string(),
            ],
        },
    );

    let mut settings = Settings::default();
    settings.agent.provider = LlmProvider::OpenRouter;
    settings.agent.model = "qwen/qwen3.6-plus".into();
    settings.providers = providers;
    settings.workspace.connection_string =
        Credential::new(workspace_dir.path().to_string_lossy().into_owned());
    settings.chat_history = ChatHistoryConfig {
        backend: "disk".into(),
        connection_string: Credential::new(
            chat_dir.path().to_string_lossy().into_owned(),
        ),
    };

    let registry = Arc::new(ClientRegistry::new(&settings, None));
    let history: Arc<dyn ChatHistory> = Arc::new(
        DiskChatHistory::new(chat_dir.path().to_path_buf()).expect("disk history"),
    );
    let feedback = Arc::new(FeedbackStore::new(chat_dir.path().to_path_buf()));

    let state = test_helpers::build_state(
        settings,
        registry,
        None,
        Some(history),
        Some(feedback),
        auth,
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let base = format!("http://{}", addr);
    let handle = tokio::spawn(test_helpers::serve(state.clone(), listener));

    // Tiny settle so the spawn is in the accept loop before the first
    // request races it.
    tokio::time::sleep(Duration::from_millis(20)).await;

    Rig {
        base,
        state,
        chat_dir,
        workspace_dir,
        _handle: handle,
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers (bare hyper — no reqwest, matches the rest of the crate)
// ---------------------------------------------------------------------------

async fn get(url: &str) -> Response<Incoming> {
    request(url, Method::GET, None).await
}

async fn get_with_header(url: &str, name: &str, value: &str) -> Response<Incoming> {
    request_with_headers(url, Method::GET, None, &[(name, value)]).await
}

async fn post_json<B: serde::Serialize>(url: &str, body: &B) -> Response<Incoming> {
    let bytes = serde_json::to_vec(body).expect("serialize");
    request(url, Method::POST, Some(bytes)).await
}

async fn request(url: &str, method: Method, body: Option<Vec<u8>>) -> Response<Incoming> {
    request_with_headers(url, method, body, &[]).await
}

async fn request_with_headers(
    url: &str,
    method: Method,
    body: Option<Vec<u8>>,
    extra_headers: &[(&str, &str)],
) -> Response<Incoming> {
    use hyper::client::conn::http1;
    // Tiny inline URL parse — every test URL is `http://127.0.0.1:N/path`,
    // so a hand-split is enough and saves pulling in the `url` crate.
    let after_scheme = url
        .strip_prefix("http://")
        .expect("test URLs are http only");
    let (authority, path_q) = match after_scheme.find('/') {
        Some(i) => (&after_scheme[..i], &after_scheme[i..]),
        None => (after_scheme, "/"),
    };
    let stream = tokio::net::TcpStream::connect(authority)
        .await
        .expect("connect");
    let io = TokioIo::new(stream);
    let (mut sender, conn) = http1::handshake(io).await.expect("handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut builder = Request::builder()
        .method(method)
        .uri(path_q)
        .header(hyper::header::HOST, authority);
    for (k, v) in extra_headers {
        builder = builder.header(*k, *v);
    }

    let req = if let Some(b) = body {
        builder = builder.header(hyper::header::CONTENT_TYPE, "application/json");
        builder.body(BodyEither::Full(Full::new(Bytes::from(b)))).expect("req")
    } else {
        builder.body(BodyEither::Empty(Empty::new())).expect("req")
    };
    sender.send_request(req).await.expect("send")
}

#[derive(Debug)]
enum BodyEither {
    Full(Full<Bytes>),
    Empty(Empty<Bytes>),
}

impl hyper::body::Body for BodyEither {
    type Data = Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        // Safety: we only forward to inner; pin-projection by hand.
        unsafe {
            match self.get_unchecked_mut() {
                BodyEither::Full(f) => std::pin::Pin::new_unchecked(f).poll_frame(cx),
                BodyEither::Empty(e) => std::pin::Pin::new_unchecked(e).poll_frame(cx),
            }
        }
    }
}

async fn body_json(resp: Response<Incoming>) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.expect("collect").to_bytes();
    serde_json::from_slice(&bytes).expect("json parse")
}

async fn body_string(resp: Response<Incoming>) -> String {
    let bytes = resp.into_body().collect().await.expect("collect").to_bytes();
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_conversations_starts_empty() {
    let r = rig().await;
    let resp = get(&format!("{}/api/conversations", r.base)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body, serde_json::json!([]));
}

#[tokio::test]
async fn create_then_list_returns_chat_with_only_real_fields() {
    let r = rig().await;
    let created = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "smoke" }),
    ).await).await;
    assert_eq!(created["title"], "smoke");
    let id = created["id"].as_str().unwrap().to_string();

    let listed = body_json(get(&format!("{}/api/conversations", r.base)).await).await;
    let list = listed.as_array().unwrap();
    assert_eq!(list.len(), 1);
    let only = &list[0];
    assert_eq!(only["id"], id);
    assert_eq!(only["title"], "smoke");
    assert_eq!(only["live"], false);
    // Fresh chat has never emitted an artefact; the Artefacts view's
    // sidebar filter relies on this being accurate.
    assert_eq!(only["has_artefacts"], false);
    // HTTP-minted id → source=http.  The sidebar badges non-http rows.
    assert_eq!(only["source"], "http");

    // No fabricated fields — assert the contract the bridge depends on.
    let keys: Vec<&str> = only.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["has_artefacts", "id", "live", "source", "title"]);
}

#[tokio::test]
async fn providers_returns_full_models_list_with_active_first() {
    let r = rig().await;
    let body = body_json(get(&format!("{}/api/providers", r.base)).await).await;
    let provs = body.as_array().unwrap();
    assert_eq!(provs.len(), 1);
    let p = &provs[0];
    assert_eq!(p["id"], "default");
    assert_eq!(p["active"], true);
    assert_eq!(p["active_model"], "qwen/qwen3.6-plus");
    let models: Vec<&str> = p["models"].as_array().unwrap().iter()
        .map(|v| v.as_str().unwrap()).collect();
    assert!(models.contains(&"qwen/qwen3.6-plus"));
    assert!(models.contains(&"minimax/minimax-m2.5"));
}

#[tokio::test]
async fn skills_endpoint_is_gone() {
    // Regression: /api/skills was removed when the Sandbox view was
    // deleted.  Hitting it must 404 rather than silently succeed.
    let r = rig().await;
    let resp = get(&format!("{}/api/skills", r.base)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mind_lists_workspace_files_and_round_trips_an_edit() {
    let r = rig().await;
    // Newly-created workspace populates SOUL.md/IDENTITY.md/etc.
    let mind = body_json(get(&format!("{}/api/mind", r.base)).await).await;
    let files: Vec<&str> = mind["files"].as_array().unwrap().iter()
        .map(|f| f["path"].as_str().unwrap()).collect();
    assert!(files.contains(&"SOUL.md"), "files = {files:?}");

    // Write through the API.
    let resp = post_json(
        &format!("{}/api/mind/file", r.base),
        &serde_json::json!({ "path": "_test.md", "content": "hello world" }),
    ).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Read back.
    let read = body_json(get(&format!("{}/api/mind/file?path=_test.md", r.base)).await).await;
    assert_eq!(read["content"], "hello world");
    assert_eq!(read["path"], "_test.md");

    // The agent's workspace and the API share storage — file lands on disk.
    let written = r.workspace_dir.path().join("_test.md");
    assert!(written.exists(), "workspace file not on disk: {written:?}");
}

#[tokio::test]
async fn feedback_round_trip_telegram_compatible() {
    let r = rig().await;
    let id = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "rate me" }),
    ).await).await["id"].as_str().unwrap().to_string();

    // Empty feedback initially.
    let initial = body_json(get(&format!("{}/api/conversations/{id}/feedback", r.base)).await).await;
    assert_eq!(initial, serde_json::json!([]));

    // Set a 👍.
    let set = body_json(post_json(
        &format!("{}/api/conversations/{id}/feedback", r.base),
        &serde_json::json!({ "turn_index": 1, "emoji": "👍" }),
    ).await).await;
    assert_eq!(set["rating"], "good");

    // Read it back.
    let entries = body_json(get(&format!("{}/api/conversations/{id}/feedback", r.base)).await).await;
    let arr = entries.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["turn_index"], 1);
    assert_eq!(arr[0]["rating"], "good");
    assert_eq!(arr[0]["score"], 1);

    // Feedback lives at {chat_dir}/{chat_id}/feedback.json — same
    // per-chat layout the Telegram controller reads from.
    let path = r.chat_dir.path().join(&id).join("feedback.json");
    assert!(path.exists(), "feedback file not on disk: {path:?}");

    // Empty emoji removes.
    post_json(
        &format!("{}/api/conversations/{id}/feedback", r.base),
        &serde_json::json!({ "turn_index": 1, "emoji": "" }),
    ).await;
    let after = body_json(get(&format!("{}/api/conversations/{id}/feedback", r.base)).await).await;
    assert_eq!(after, serde_json::json!([]));
}

#[tokio::test]
async fn unknown_emoji_is_rejected_400() {
    let r = rig().await;
    let id = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "x" }),
    ).await).await["id"].as_str().unwrap().to_string();
    let resp = post_json(
        &format!("{}/api/conversations/{id}/feedback", r.base),
        &serde_json::json!({ "turn_index": 0, "emoji": "🦀" }),
    ).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn embedded_static_assets_serve_with_correct_content_types() {
    let r = rig().await;
    let cases: &[(&str, &str)] = &[
        ("/", "text/html"),
        ("/styles/tokens.css", "text/css"),
        ("/js/bridge.js", "application/javascript"),
        ("/components/app.jsx", "text/babel"),
    ];
    for (path, want_ct) in cases {
        let resp = get(&format!("{}{}", r.base, path)).await;
        assert_eq!(resp.status(), StatusCode::OK, "GET {path}");
        let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.starts_with(want_ct), "content-type for {path} = {ct}");
        let body = body_string(resp).await;
        assert!(!body.is_empty(), "empty body for {path}");
    }
}

#[tokio::test]
async fn unknown_route_returns_404_not_method_not_allowed() {
    let r = rig().await;
    let resp = get(&format!("{}/api/this-does-not-exist", r.base)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_chat_appears_at_top_of_list() {
    // Regression for "newest-first" ordering: a freshly-created chat
    // must be the first list entry, before any prior ones.
    let r = rig().await;
    for title in ["first", "second", "third"] {
        post_json(
            &format!("{}/api/conversations", r.base),
            &serde_json::json!({ "title": title }),
        ).await;
    }
    let listed = body_json(get(&format!("{}/api/conversations", r.base)).await).await;
    let titles: Vec<&str> = listed.as_array().unwrap().iter()
        .map(|c| c["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["third", "second", "first"]);
}

#[tokio::test]
async fn cancel_unknown_chat_returns_404() {
    let r = rig().await;
    let resp = post_json(
        &format!("{}/api/conversations/c-nope/cancel", r.base),
        &serde_json::json!({}),
    ).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn activity_endpoint_is_honest_about_being_empty() {
    // /api/activity returns an empty list because cross-controller
    // BackgroundAgentRegistry aggregation doesn't exist yet.  The
    // endpoint must not invent fake activity to fill the page.
    let r = rig().await;
    let body = body_json(get(&format!("{}/api/activity", r.base)).await).await;
    assert_eq!(body["lanes"].as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// More API surface — extra coverage rather than smoke
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_conversation_404_for_unknown_id() {
    let r = rig().await;
    let resp = get(&format!("{}/api/conversations/c-nope", r.base)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_conversation_returns_empty_messages_for_new_chat() {
    let r = rig().await;
    let id = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "fresh" }),
    ).await).await["id"].as_str().unwrap().to_string();

    let body = body_json(get(&format!("{}/api/conversations/{id}", r.base)).await).await;
    assert_eq!(body["id"], id);
    assert_eq!(body["title"], "fresh");
    assert!(body["messages"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn get_feedback_for_unknown_chat_returns_empty_list() {
    // FeedbackStore::load returns Ok(empty) when the file doesn't
    // exist; no need to check chat existence.  Match that.
    let r = rig().await;
    let body = body_json(get(&format!("{}/api/conversations/c-nope/feedback", r.base)).await).await;
    assert_eq!(body, serde_json::json!([]));
}

#[tokio::test]
async fn cancel_unknown_chat_404_but_known_chat_idempotent() {
    let r = rig().await;
    // Unknown → 404
    let bad = post_json(
        &format!("{}/api/conversations/c-missing/cancel", r.base),
        &serde_json::json!({}),
    ).await;
    assert_eq!(bad.status(), StatusCode::NOT_FOUND);
    // Known but no turn running → still 200 (idempotent)
    let id = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "x" }),
    ).await).await["id"].as_str().unwrap().to_string();
    let ok = post_json(
        &format!("{}/api/conversations/{id}/cancel", r.base),
        &serde_json::json!({}),
    ).await;
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(body_json(ok).await["ok"], true);
}

#[tokio::test]
async fn post_turn_404_for_unknown_chat() {
    let r = rig().await;
    let resp = post_json(
        &format!("{}/api/conversations/c-nope/turn", r.base),
        &serde_json::json!({ "prompt": "hi" }),
    ).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn post_turn_400_for_invalid_body() {
    let r = rig().await;
    let id = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "x" }),
    ).await).await["id"].as_str().unwrap().to_string();
    // Missing required `prompt` field — JSON parse fails → 400.
    let resp = post_json(
        &format!("{}/api/conversations/{id}/turn", r.base),
        &serde_json::json!({}),
    ).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_conversation_400_for_invalid_body() {
    let r = rig().await;
    let resp = request(
        &format!("{}/api/conversations", r.base),
        Method::POST,
        Some(b"not json at all".to_vec()),
    ).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_model_400_for_unknown_provider() {
    let r = rig().await;
    let resp = post_json(
        &format!("{}/api/model", r.base),
        &serde_json::json!({ "provider": "does-not-exist" }),
    ).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_model_returns_zero_swapped_when_no_agents_loaded() {
    // No chats have a built agent yet — the swap is a no-op but still
    // succeeds; clients use `swapped` to decide whether the change
    // took effect on any in-flight session.
    let r = rig().await;
    let body = body_json(post_json(
        &format!("{}/api/model", r.base),
        &serde_json::json!({ "provider": "default", "model": "minimax/minimax-m2.5" }),
    ).await).await;
    assert_eq!(body["ok"], true);
    assert_eq!(body["model"], "minimax/minimax-m2.5");
    assert_eq!(body["swapped"], 0);
}

#[tokio::test]
async fn post_model_targets_specific_chat_404_when_unknown() {
    let r = rig().await;
    let resp = post_json(
        &format!("{}/api/model", r.base),
        &serde_json::json!({
            "provider": "default",
            "chat_id": "c-missing",
        }),
    ).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn post_mind_file_400_when_payload_invalid() {
    let r = rig().await;
    // Missing `path` field — serde rejects, controller returns 400.
    let resp = post_json(
        &format!("{}/api/mind/file", r.base),
        &serde_json::json!({ "content": "x" }),
    ).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_mind_file_400_when_path_query_missing() {
    let r = rig().await;
    let resp = get(&format!("{}/api/mind/file", r.base)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_mind_file_404_for_unknown_file() {
    let r = rig().await;
    let resp = get(&format!("{}/api/mind/file?path=this-file-does-not-exist.md", r.base)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sse_endpoint_serves_event_stream() {
    // Open the SSE endpoint and verify the headers are right —
    // bridge.js's EventSource won't auto-reconnect properly without
    // text/event-stream + no-cache.
    let r = rig().await;
    let id = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "sse" }),
    ).await).await["id"].as_str().unwrap().to_string();
    let resp = get(&format!("{}/api/conversations/{id}/events", r.base)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap().to_str().unwrap(),
        "text/event-stream",
    );
    assert_eq!(
        resp.headers().get("cache-control").unwrap().to_str().unwrap(),
        "no-cache",
    );
    // Don't drain the body — it's an open broadcast subscriber and
    // would block forever waiting for a turn we're not sending.
}

#[tokio::test]
async fn static_path_traversal_is_blocked() {
    // The controller refuses any path containing "..", so a client
    // can't escape the embedded asset table when an on-disk webroot
    // is configured.  Returns 404, not 403, on purpose — same shape
    // as a missing asset.
    let r = rig().await;
    for evil in [
        "/../../../../etc/passwd",
        "/styles/../../etc/hosts",
        "/components/..%2Fapp.jsx",  // url-decoded `..` would still fail
    ] {
        let resp = get(&format!("{}{}", r.base, evil)).await;
        assert!(
            resp.status() == StatusCode::NOT_FOUND
                || resp.status() == StatusCode::BAD_REQUEST,
            "GET {} returned {}",
            evil,
            resp.status(),
        );
    }
}

#[tokio::test]
async fn static_unknown_asset_returns_404() {
    let r = rig().await;
    let resp = get(&format!("{}/styles/does-not-exist.css", r.base)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unsupported_method_returns_405() {
    // PUT isn't matched by any route; falls through to the static
    // handler which only serves GET; falls through to the bottom-of-
    // dispatch fallback which returns 405.
    let r = rig().await;
    let resp = request(
        &format!("{}/api/conversations", r.base),
        Method::PUT,
        Some(b"{}".to_vec()),
    ).await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn concurrent_creates_get_distinct_ids() {
    // mint_id() must not race — two parallel POSTs got back the same
    // id during an earlier refactor.  Spawn 16 concurrent creates and
    // make sure every returned id is unique.
    let r = rig().await;
    let mut handles = Vec::new();
    for i in 0..16 {
        let url = format!("{}/api/conversations", r.base);
        let body = serde_json::json!({ "title": format!("c{i}") });
        handles.push(tokio::spawn(async move {
            let resp = post_json(&url, &body).await;
            body_json(resp).await["id"].as_str().unwrap().to_string()
        }));
    }
    let mut ids = Vec::new();
    for h in handles {
        ids.push(h.await.unwrap());
    }
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "duplicate ids: {ids:?}");
}

#[tokio::test]
async fn feedback_overwrites_existing_rating_for_same_turn() {
    // Same turn rated twice → second wins (latest reaction is the one
    // stored; the FeedbackStore uses upsert).
    let r = rig().await;
    let id = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "swap" }),
    ).await).await["id"].as_str().unwrap().to_string();
    post_json(
        &format!("{}/api/conversations/{id}/feedback", r.base),
        &serde_json::json!({ "turn_index": 1, "emoji": "👍" }),
    ).await;
    post_json(
        &format!("{}/api/conversations/{id}/feedback", r.base),
        &serde_json::json!({ "turn_index": 1, "emoji": "🔥" }),
    ).await;
    let entries = body_json(get(&format!("{}/api/conversations/{id}/feedback", r.base)).await).await;
    let arr = entries.as_array().unwrap();
    assert_eq!(arr.len(), 1, "should have replaced not appended");
    assert_eq!(arr[0]["rating"], "very_good");
}

#[tokio::test]
async fn post_turn_with_slash_clear_rotates_chat_history() {
    // Regression: /clear is a listed controller command in the web
    // composer's slash menu (data.js), but post_turn did not intercept
    // it — the prompt went straight to the LLM and nothing on disk
    // changed.  Hitting /clear must rotate the chat (archive the
    // current transcript, reset the current file) the same way the
    // Telegram controller does via execute_agent_command.
    let r = rig().await;

    let id = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "seed" }),
    ).await).await["id"].as_str().unwrap().to_string();

    // Seed the on-disk transcript so rotation has something real to
    // archive.  (The controller calls save(id, &[]) during create, so
    // an empty file already exists — we overwrite it with a message.)
    let store = DiskChatHistory::new(r.chat_dir.path().to_path_buf())
        .expect("seed store");
    store
        .save(&id, &[dyson::message::Message::user("hello world")])
        .expect("seed save");

    let current = r.chat_dir.path().join(&id).join("transcript.json");
    let before: Vec<dyson::message::Message> =
        serde_json::from_str(&std::fs::read_to_string(&current).unwrap()).unwrap();
    assert_eq!(before.len(), 1, "pre-clear transcript should have 1 message");

    // POST /clear — must return synchronously (no agent spawn) with a
    // 2xx.  The current file is empty afterwards and exactly one
    // rotated archive holds the original message.
    let resp = post_json(
        &format!("{}/api/conversations/{id}/turn", r.base),
        &serde_json::json!({ "prompt": "/clear" }),
    ).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["cleared"], true);

    // The current file must still exist (empty) so DiskChatHistory::list
    // keeps the chat visible on restart — otherwise the rotation would
    // strand any artefacts filtered by this chat_id.
    assert!(current.exists(), "current file must be re-seeded empty after rotate");
    let after: Vec<dyson::message::Message> =
        serde_json::from_str(&std::fs::read_to_string(&current).unwrap()).unwrap();
    assert!(after.is_empty(), "current transcript should be cleared");

    let archives = r.chat_dir.path().join(&id).join("archives");
    let rotated: Vec<_> = std::fs::read_dir(&archives)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .collect();
    assert_eq!(
        rotated.len(),
        1,
        "exactly one rotated archive expected — found: {:?}",
        rotated.iter().map(|e| e.file_name()).collect::<Vec<_>>(),
    );
    let rotated_msgs: Vec<dyson::message::Message> =
        serde_json::from_str(&std::fs::read_to_string(rotated[0].path()).unwrap()).unwrap();
    assert_eq!(rotated_msgs.len(), 1, "archive preserves prior transcript");
}

#[tokio::test]
async fn create_conversation_rotates_previous_chat_when_requested() {
    // Regression: clicking "+ New Conversation" in the web sidebar is
    // supposed to archive the currently-active chat the same way
    // /clear does — otherwise the "separate chat" the user thinks
    // they're starting shares disk storage with the previous one and
    // looks identical on reload.  The create endpoint accepts an
    // optional `rotate_previous` id and rotates that chat's file
    // before minting the new one.
    let r = rig().await;

    let prev = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "old" }),
    ).await).await["id"].as_str().unwrap().to_string();

    let store = DiskChatHistory::new(r.chat_dir.path().to_path_buf())
        .expect("seed store");
    store
        .save(&prev, &[dyson::message::Message::user("first thought")])
        .expect("seed save");

    let resp = post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({
            "title": "new",
            "rotate_previous": prev,
        }),
    ).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let created = body_json(resp).await;
    assert_ne!(created["id"].as_str().unwrap(), prev);

    // Old chat's current file is empty (or missing); exactly one
    // rotated archive sits alongside it holding the seeded message.
    let old_current = r.chat_dir.path().join(&prev).join("transcript.json");
    // Empty current file must remain so the rotated chat stays
    // enumerable — list() skips rotated archives, so without this the
    // chat (and its artefacts) would vanish from the sidebar on restart.
    assert!(
        old_current.exists(),
        "prev chat's current file must be re-seeded empty after rotate",
    );
    let after: Vec<dyson::message::Message> =
        serde_json::from_str(&std::fs::read_to_string(&old_current).unwrap()).unwrap();
    assert!(after.is_empty(), "prev chat's current file should be cleared");

    let archives = r.chat_dir.path().join(&prev).join("archives");
    let rotated: Vec<_> = std::fs::read_dir(&archives)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .collect();
    assert_eq!(rotated.len(), 1, "one rotated archive expected for prev chat");
}

#[tokio::test]
async fn mint_id_skips_ids_used_by_rotated_archives_or_orphan_artefacts() {
    // Regression: a freshly minted chat was inheriting artefacts from
    // a prior chat_id because `mint_id` only checked in-memory chats.
    // If c-0042 was rotated away (only archive survives) or its
    // current file was deleted but an artefact still carries
    // chat_id=c-0042, the next create must NOT reuse c-0042 — or the
    // new empty chat surfaces someone else's generated images.
    let chat_dir = tempfile::tempdir().expect("chat tempdir");
    let workspace_dir = tempfile::tempdir().expect("workspace tempdir");

    // Seed a rotated archive for c-0042 (no current file).
    std::fs::write(
        chat_dir.path().join("c-0042.2026-04-22T12-00-00.json"),
        b"[]",
    ).unwrap();

    // Seed an orphan artefact that claims chat_id=c-0099.
    let art_dir = chat_dir.path().join("artefacts");
    std::fs::create_dir_all(&art_dir).unwrap();
    std::fs::write(art_dir.join("a1.body"), b"/api/files/f1").unwrap();
    std::fs::write(
        art_dir.join("a1.meta.json"),
        br#"{"chat_id":"c-0099","created_at":0,"kind":"image","title":"stray.png","mime_type":"image/png"}"#,
    ).unwrap();

    // Build the rig with our pre-seeded dirs so startup hydration sees them.
    let mut providers = std::collections::HashMap::new();
    providers.insert(
        "default".to_string(),
        ProviderConfig {
            provider_type: LlmProvider::OpenRouter,
            api_key: Credential::new("sk-test".into()),
            base_url: None,
            models: vec!["qwen/qwen3.6-plus".into()],
        },
    );
    let mut settings = Settings::default();
    settings.agent.provider = LlmProvider::OpenRouter;
    settings.agent.model = "qwen/qwen3.6-plus".into();
    settings.providers = providers;
    settings.workspace.connection_string =
        Credential::new(workspace_dir.path().to_string_lossy().into_owned());
    settings.chat_history = ChatHistoryConfig {
        backend: "disk".into(),
        connection_string: Credential::new(chat_dir.path().to_string_lossy().into_owned()),
    };
    let registry = Arc::new(ClientRegistry::new(&settings, None));
    let history: Arc<dyn ChatHistory> = Arc::new(
        DiskChatHistory::new(chat_dir.path().to_path_buf()).expect("disk history"),
    );
    let feedback = Arc::new(FeedbackStore::new(chat_dir.path().to_path_buf()));
    let state = test_helpers::build_state(
        settings, registry, None, Some(history), Some(feedback),
        Arc::new(DangerousNoAuth),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{}", addr);
    let _handle = tokio::spawn(test_helpers::serve(state.clone(), listener));
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Mint enough ids to walk past 0042 and 0099 — 110 calls is
    // generous and catches both in one sweep.  Any reuse would mean
    // the next conversation silently inherits a5's artefact list.
    let mut minted = Vec::new();
    for _ in 0..110 {
        let body = body_json(post_json(
            &format!("{}/api/conversations", base),
            &serde_json::json!({ "title": "x" }),
        ).await).await;
        minted.push(body["id"].as_str().unwrap().to_string());
    }
    assert!(
        !minted.contains(&"c-0042".to_string()),
        "mint must skip rotated-archive ids — minted: {minted:?}",
    );
    assert!(
        !minted.contains(&"c-0099".to_string()),
        "mint must skip ids carried by orphan artefacts — minted: {minted:?}",
    );
    // Keep dirs alive until here.
    drop(chat_dir);
    drop(workspace_dir);
}

#[tokio::test]
async fn delete_empty_chat_removes_file_and_drops_from_list() {
    // Empty chat → hard delete.  No rotated archive, no current file;
    // a freshly-minted conversation the user immediately removes
    // shouldn't leave a zero-byte `[]` file stranded on disk.
    let r = rig().await;
    let id = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "trash me" }),
    ).await).await["id"].as_str().unwrap().to_string();
    let chat_root = r.chat_dir.path().join(&id);
    let current = chat_root.join("transcript.json");
    assert!(current.exists(), "create should seed an empty current file");

    let resp = request(
        &format!("{}/api/conversations/{id}", r.base),
        Method::DELETE,
        None,
    ).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["deleted"], true);
    assert_eq!(body["preserved"], false);

    // Cascade delete: the whole chat subdir is gone.
    assert!(!chat_root.exists(), "empty chat's subdir should be gone");
    let listed = body_json(get(&format!("{}/api/conversations", r.base)).await).await;
    let ids: Vec<&str> = listed.as_array().unwrap().iter()
        .map(|c| c["id"].as_str().unwrap()).collect();
    assert!(!ids.contains(&id.as_str()), "deleted chat must not appear in list");
}

#[tokio::test]
async fn delete_non_empty_chat_rotates_then_drops_from_list() {
    // Non-empty chat → keep the transcript on disk (as a dated
    // archive) but drop the chat from the sidebar.  Same shape
    // `/clear` produces, without the re-seeded current file.
    let r = rig().await;
    let id = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "keep" }),
    ).await).await["id"].as_str().unwrap().to_string();

    let store = DiskChatHistory::new(r.chat_dir.path().to_path_buf()).expect("seed store");
    store
        .save(&id, &[dyson::message::Message::user("dont lose this")])
        .expect("seed save");

    let resp = request(
        &format!("{}/api/conversations/{id}", r.base),
        Method::DELETE,
        None,
    ).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["deleted"], true);
    assert_eq!(body["preserved"], true);

    let current = r.chat_dir.path().join(&id).join("transcript.json");
    assert!(!current.exists(), "current file is archived away, not re-seeded");
    let archives = r.chat_dir.path().join(&id).join("archives");
    let rotated: Vec<_> = std::fs::read_dir(&archives)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .collect();
    assert_eq!(rotated.len(), 1, "one dated archive preserves the transcript");
    let archived: Vec<dyson::message::Message> =
        serde_json::from_str(&std::fs::read_to_string(rotated[0].path()).unwrap()).unwrap();
    assert_eq!(archived.len(), 1, "archive still holds the original message");

    let listed = body_json(get(&format!("{}/api/conversations", r.base)).await).await;
    let ids: Vec<&str> = listed.as_array().unwrap().iter()
        .map(|c| c["id"].as_str().unwrap()).collect();
    assert!(!ids.contains(&id.as_str()), "deleted chat must not appear in list");
}

#[tokio::test]
async fn delete_unknown_chat_returns_404() {
    let r = rig().await;
    let resp = request(
        &format!("{}/api/conversations/c-missing", r.base),
        Method::DELETE,
        None,
    ).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn turn_with_attachments_accepts_base64_payload() {
    // Upload path: POST /turn with an `attachments` array.  The
    // controller decodes base64 up front and returns 202 — failures
    // would land as 400 before kicking off the agent.
    use base64::Engine;
    let r = rig().await;
    let id = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "upload" }),
    ).await).await["id"].as_str().unwrap().to_string();

    let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\n");
    let resp = post_json(
        &format!("{}/api/conversations/{id}/turn", r.base),
        &serde_json::json!({
            "prompt": "what is this?",
            "attachments": [{
                "name": "tiny.png",
                "mime_type": "image/png",
                "data_base64": png,
            }],
        }),
    ).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn turn_with_invalid_base64_attachment_400s_clean() {
    // Malformed base64 must reject BEFORE we kick off the agent —
    // otherwise the user gets a 202 + an SSE error a second later
    // (and the agent already started).
    let r = rig().await;
    let id = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "bad upload" }),
    ).await).await["id"].as_str().unwrap().to_string();
    let resp = post_json(
        &format!("{}/api/conversations/{id}/turn", r.base),
        &serde_json::json!({
            "prompt": "x",
            "attachments": [{
                "name": "bad.bin",
                "mime_type": "application/octet-stream",
                "data_base64": "!!! not base64 !!!",
            }],
        }),
    ).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn files_endpoint_404s_for_unknown_id() {
    // /api/files/<id> serves agent-produced files (image_generate
    // outputs etc.).  Unknown id is a 404 — same shape as missing
    // static asset.
    let r = rig().await;
    let resp = get(&format!("{}/api/files/does-not-exist", r.base)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn root_path_serves_prototype_html() {
    // GET / must serve the prototype, not redirect or 404.
    let r = rig().await;
    let resp = get(&format!("{}/", r.base)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("text/html"));
    let html = body_string(resp).await;
    assert!(html.contains("<div id=\"root\">"));
    assert!(html.contains("js/bridge.js"), "must load bridge.js");
}

// ---------------------------------------------------------------------------
// Auth — DangerousNoAuth is exercised implicitly by every test above; here
// we lock in the Bearer-protected path and the static-shell exemption.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bearer_auth_rejects_unauthenticated_api_request() {
    let auth = Arc::new(BearerTokenAuth::new("s3cret".into()));
    let r = rig_with_auth(auth).await;
    let resp = get(&format!("{}/api/conversations", r.base)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "unauthorized");
}

#[tokio::test]
async fn bearer_auth_rejects_wrong_token() {
    let auth = Arc::new(BearerTokenAuth::new("correct".into()));
    let r = rig_with_auth(auth).await;
    let resp = get_with_header(
        &format!("{}/api/conversations", r.base),
        "authorization",
        "Bearer wrong",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bearer_auth_accepts_matching_token() {
    let auth = Arc::new(BearerTokenAuth::new("right-token".into()));
    let r = rig_with_auth(auth).await;
    let resp = get_with_header(
        &format!("{}/api/conversations", r.base),
        "authorization",
        "Bearer right-token",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn bearer_auth_still_serves_static_shell_without_token() {
    // The UI has to load before the browser can present any credential,
    // so `/`, `/styles/*`, `/js/*`, `/components/*` are exempt.  Without
    // this the prototype would 401 on the very first GET /.
    let auth = Arc::new(BearerTokenAuth::new("s3cret".into()));
    let r = rig_with_auth(auth).await;
    for path in ["/", "/styles/tokens.css", "/js/bridge.js", "/components/app.jsx"] {
        let resp = get(&format!("{}{}", r.base, path)).await;
        assert_eq!(resp.status(), StatusCode::OK, "GET {path} must be exempt");
    }
}

#[tokio::test]
async fn http_controller_rejects_non_loopback_bind_without_auth() {
    // Non-loopback bind + missing `auth` → from_config returns None.
    // This is the guardrail against silently exposing an
    // unauthenticated endpoint to the network.
    use dyson::config::ControllerConfig;
    use dyson::controller::http::HttpController;

    let cfg = ControllerConfig {
        controller_type: "http".into(),
        config: serde_json::json!({
            "bind": "0.0.0.0:7878",
        }),
    };
    assert!(HttpController::from_config(&cfg).is_none());
}

#[tokio::test]
async fn http_controller_allows_loopback_bind_without_auth() {
    // Loopback bind + missing `auth` → defaults to DangerousNoAuth.
    // The loopback threat model is a single trusted operator, so this
    // preserves the existing dev ergonomics of just writing
    // `{ "type": "http", "bind": "127.0.0.1:7878" }`.
    use dyson::config::ControllerConfig;
    use dyson::controller::http::HttpController;

    for bind in ["127.0.0.1:0", "127.0.0.1:7878", "[::1]:0"] {
        let cfg = ControllerConfig {
            controller_type: "http".into(),
            config: serde_json::json!({ "bind": bind }),
        };
        assert!(
            HttpController::from_config(&cfg).is_some(),
            "loopback bind {bind} should default to DangerousNoAuth",
        );
    }
}

#[tokio::test]
async fn http_controller_accepts_dangerous_no_auth_config() {
    use dyson::config::ControllerConfig;
    use dyson::controller::http::HttpController;

    let cfg = ControllerConfig {
        controller_type: "http".into(),
        config: serde_json::json!({
            "bind": "127.0.0.1:0",
            "auth": { "type": "dangerous_no_auth" },
        }),
    };
    assert!(HttpController::from_config(&cfg).is_some());
}

#[tokio::test]
async fn http_controller_accepts_bearer_config() {
    use dyson::config::ControllerConfig;
    use dyson::controller::http::HttpController;

    let cfg = ControllerConfig {
        controller_type: "http".into(),
        config: serde_json::json!({
            "bind": "127.0.0.1:0",
            "auth": { "type": "bearer", "token": "abc123" },
        }),
    };
    assert!(HttpController::from_config(&cfg).is_some());
}

#[tokio::test]
async fn http_controller_rejects_bearer_with_empty_token() {
    use dyson::config::ControllerConfig;
    use dyson::controller::http::HttpController;

    let cfg = ControllerConfig {
        controller_type: "http".into(),
        config: serde_json::json!({
            "bind": "127.0.0.1:0",
            "auth": { "type": "bearer", "token": "" },
        }),
    };
    assert!(HttpController::from_config(&cfg).is_none());
}

// ---------------------------------------------------------------------------
// Agent-produced file delivery (image inline, everything else as download)
// ---------------------------------------------------------------------------

/// Block on the next `data: {...}\n\n` SSE record on a live stream and
/// return the parsed JSON payload.  Accumulates across frame boundaries
/// because hyper doesn't guarantee one SSE record per body frame.
async fn read_sse_event(resp: &mut Response<Incoming>) -> serde_json::Value {
    let mut buf = String::new();
    loop {
        let frame = resp
            .body_mut()
            .frame()
            .await
            .expect("stream ended before SSE event")
            .expect("frame error");
        let data = match frame.into_data() {
            Ok(d) => d,
            Err(_) => continue, // trailers frame — skip
        };
        buf.push_str(std::str::from_utf8(&data).expect("sse utf-8"));
        while let Some(i) = buf.find("\n\n") {
            let record: String = buf.drain(..i + 2).collect();
            let record = record.trim_end_matches('\n');
            if let Some(payload) = record.strip_prefix("data: ") {
                return serde_json::from_str(payload).expect("sse json");
            }
            // Comment-only frames (": lag\n\n") or empty records → keep
            // reading for the real event.
        }
    }
}

/// Smallest valid PNG — 1×1 transparent pixel.  The server routes on
/// extension, not magic bytes, so any payload works; using a real PNG
/// is just a courtesy to anyone who tails a failing test's temp dir.
const PNG_1X1: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D',
    b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
    0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, b'I', b'D', b'A', b'T', 0x78, 0x9c, 0x62, 0x00,
    0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, b'I',
    b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82,
];

#[tokio::test]
async fn send_file_inlines_images_and_attaches_everything_else() {
    // End-to-end: agent emits a file via `Output::send_file` → SSE `file`
    // event with `inline_image` set per MIME → `/api/files/<id>` serves
    // with the matching `Content-Disposition`.  Covers the contract the
    // web UI's `FileBlock` depends on (inline `<img>` for images,
    // download card for everything else).
    let r = rig().await;

    // Create a chat so there's a broadcast channel to publish on.
    let created = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "files" }),
    ).await).await;
    let id = created["id"].as_str().unwrap().to_string();

    // Open the SSE stream BEFORE emitting — broadcast drops events
    // with zero receivers, so we need the subscription to land first.
    let mut sse = request(
        &format!("{}/api/conversations/{}/events", r.base, id),
        Method::GET,
        None,
    ).await;
    assert_eq!(sse.status(), StatusCode::OK);
    assert_eq!(
        sse.headers().get("content-type").unwrap().to_str().unwrap(),
        "text/event-stream",
    );
    test_helpers::wait_for_sse_subscriber(r.state.clone(), &id).await;

    let dir = tempfile::tempdir().expect("tempdir");

    // --- PNG: must be delivered as inline image ---
    let png_path = dir.path().join("chart.png");
    std::fs::write(&png_path, PNG_1X1).expect("write png");
    test_helpers::emit_agent_file(r.state.clone(), &id, &png_path)
        .await
        .expect("emit png");

    let evt = read_sse_event(&mut sse).await;
    assert_eq!(evt["type"], "file", "event type: {evt}");
    assert_eq!(evt["name"], "chart.png");
    assert_eq!(evt["mime_type"], "image/png");
    assert_eq!(evt["inline_image"], true, "images must be flagged inline");
    let png_url = evt["url"].as_str().expect("url").to_string();
    assert!(png_url.starts_with("/api/files/"), "url shape: {png_url}");

    // Images are also discoverable in the Artefacts tab — consume the
    // follow-up artefact event so subsequent reads see the next file.
    let art = read_sse_event(&mut sse).await;
    assert_eq!(art["type"], "artefact", "images must also emit artefact: {art}");
    assert_eq!(art["kind"], "image");
    assert_eq!(art["title"], "chart.png");

    let resp = get(&format!("{}{}", r.base, png_url)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap().to_str().unwrap(),
        "image/png",
    );
    let cd = resp
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        cd.starts_with("inline;"),
        "image must be served inline, got: {cd:?}",
    );
    assert!(cd.contains("chart.png"), "filename missing from disposition: {cd:?}");
    let body = resp.into_body().collect().await.expect("collect").to_bytes();
    assert_eq!(&body[..], PNG_1X1, "served bytes must match what was emitted");

    // --- PDF: must be delivered as an attachment ---
    let pdf_path = dir.path().join("report.pdf");
    std::fs::write(&pdf_path, b"%PDF-1.4\n% not a real pdf\n").expect("write pdf");
    test_helpers::emit_agent_file(r.state.clone(), &id, &pdf_path)
        .await
        .expect("emit pdf");

    let evt = read_sse_event(&mut sse).await;
    assert_eq!(evt["type"], "file");
    assert_eq!(evt["name"], "report.pdf");
    assert_eq!(evt["mime_type"], "application/pdf");
    assert_eq!(evt["inline_image"], false, "non-images must NOT be inline");
    let pdf_url = evt["url"].as_str().expect("url").to_string();

    let resp = get(&format!("{}{}", r.base, pdf_url)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap().to_str().unwrap(),
        "application/pdf",
    );
    let cd = resp
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        cd.starts_with("attachment;"),
        "non-image must be served as attachment, got: {cd:?}",
    );
    assert!(cd.contains("report.pdf"), "filename missing from disposition: {cd:?}");
}

// ---------------------------------------------------------------------------
// Artefact delivery round-trip (Output::send_artefact → SSE → GET)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent_artefact_round_trips_through_sse_and_disk() {
    // End-to-end: agent emits an artefact via `Output::send_artefact` →
    // SSE `artefact` event → `/api/artefacts/<id>` serves the markdown
    // body with the right mime → the per-chat listing endpoint shows
    // the entry → a fresh HttpState pointed at the same directory
    // rehydrates the artefact (so the report survives controller
    // restarts and is reachable from other browser profiles).
    let r = rig().await;

    let created = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "artefacts" }),
    ).await).await;
    let id = created["id"].as_str().unwrap().to_string();

    let mut sse = request(
        &format!("{}/api/conversations/{}/events", r.base, id),
        Method::GET,
        None,
    ).await;
    assert_eq!(sse.status(), StatusCode::OK);
    test_helpers::wait_for_sse_subscriber(r.state.clone(), &id).await;

    let markdown = "# Security review: juice-shop\n\nFindings go here.\n";
    let artefact = dyson::message::Artefact::markdown(
        dyson::message::ArtefactKind::SecurityReview,
        "Security review: juice-shop",
        markdown,
    )
    .with_metadata(serde_json::json!({
        "model": "claude-opus-4-7",
        "input_tokens": 120_000,
        "output_tokens": 8_000,
    }));

    test_helpers::emit_agent_artefact(r.state.clone(), &id, artefact)
        .await
        .expect("emit artefact");

    let evt = read_sse_event(&mut sse).await;
    assert_eq!(evt["type"], "artefact", "event type: {evt}");
    assert_eq!(evt["title"], "Security review: juice-shop");
    assert_eq!(evt["kind"], "security_review");
    assert_eq!(evt["bytes"], markdown.len());
    // The emitted `url` is a shareable SPA deep-link, not the raw
    // bytes endpoint — cmd-click / copy-paste on the chip should land
    // in the reader, not download markdown.  The body still lives at
    // `/api/artefacts/<id>` and the client fetches it from there.
    let url = evt["url"].as_str().expect("url").to_string();
    assert!(url.starts_with("/#/artefacts/"), "url shape: {url}");
    let art_id = url.trim_start_matches("/#/artefacts/").to_string();

    // GET the body — must come back verbatim with text/markdown.
    let resp = get(&format!("{}/api/artefacts/{}", r.base, art_id)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().get("content-type").unwrap().to_str().unwrap()
            .starts_with("text/markdown"),
    );
    let body = resp.into_body().collect().await.expect("collect").to_bytes();
    assert_eq!(std::str::from_utf8(&body[..]).unwrap(), markdown);

    // Per-chat listing must include the artefact with its metadata.
    let list = body_json(get(&format!(
        "{}/api/conversations/{}/artefacts",
        r.base, id,
    )).await).await;
    let arr = list.as_array().expect("artefact list is array");
    assert_eq!(arr.len(), 1, "one artefact expected, got: {list}");
    assert_eq!(arr[0]["id"], art_id);
    assert_eq!(arr[0]["title"], "Security review: juice-shop");
    assert_eq!(arr[0]["kind"], "security_review");
    assert_eq!(arr[0]["metadata"]["model"], "claude-opus-4-7");

    // Disk persistence: the underlying bytes must be reachable from a
    // fresh HttpState that has never held the artefact in memory —
    // this is the "different browser profile" / "server restart"
    // scenario.  Build a second state over the same directory and
    // confirm the artefact rehydrates.
    let mut rebuilt_settings = Settings::default();
    rebuilt_settings.chat_history = ChatHistoryConfig {
        backend: "disk".into(),
        connection_string: Credential::new(
            r.chat_dir.path().to_string_lossy().into_owned(),
        ),
    };
    rebuilt_settings.workspace.connection_string =
        Credential::new(r.workspace_dir.path().to_string_lossy().into_owned());
    let mut providers = std::collections::HashMap::new();
    providers.insert(
        "default".to_string(),
        ProviderConfig {
            provider_type: LlmProvider::OpenRouter,
            api_key: Credential::new("sk-test".into()),
            base_url: None,
            models: vec!["qwen/qwen3.6-plus".to_string()],
        },
    );
    rebuilt_settings.providers = providers;
    rebuilt_settings.agent.provider = LlmProvider::OpenRouter;
    rebuilt_settings.agent.model = "qwen/qwen3.6-plus".into();

    let registry2 = Arc::new(ClientRegistry::new(&rebuilt_settings, None));
    let history2: Arc<dyn ChatHistory> = Arc::new(
        DiskChatHistory::new(r.chat_dir.path().to_path_buf()).expect("disk history"),
    );
    let feedback2 = Arc::new(FeedbackStore::new(r.chat_dir.path().to_path_buf()));
    let state2 = test_helpers::build_state(
        rebuilt_settings,
        registry2,
        None,
        Some(history2),
        Some(feedback2),
        Arc::new(DangerousNoAuth),
    );
    // The hydrate path populates `items` directly, so the body is
    // readable via the normal store access.
    let stored = state2
        .artefacts_for_test(&art_id)
        .expect("artefact rehydrated from disk");
    assert_eq!(stored, markdown);
}

#[tokio::test]
async fn list_conversations_flags_chats_with_artefacts() {
    // The Artefacts view filters the sidebar to chats that have at
    // least one report — `/api/conversations` must surface the flag
    // for that filter to work.  Create two chats, emit into one, and
    // assert only that one reports `has_artefacts`.
    let r = rig().await;

    let a = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "has reports" }),
    ).await).await;
    let a_id = a["id"].as_str().unwrap().to_string();
    let b = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "no reports" }),
    ).await).await;
    let b_id = b["id"].as_str().unwrap().to_string();

    let mut sse = request(
        &format!("{}/api/conversations/{}/events", r.base, a_id),
        Method::GET,
        None,
    ).await;
    assert_eq!(sse.status(), StatusCode::OK);
    test_helpers::wait_for_sse_subscriber(r.state.clone(), &a_id).await;
    test_helpers::emit_agent_artefact(
        r.state.clone(),
        &a_id,
        dyson::message::Artefact::markdown(
            dyson::message::ArtefactKind::SecurityReview,
            "R",
            "body",
        ),
    )
    .await
    .expect("emit");
    let _ = read_sse_event(&mut sse).await;

    let listed = body_json(get(&format!("{}/api/conversations", r.base)).await).await;
    let rows = listed.as_array().unwrap();
    let find = |id: &str| rows.iter().find(|c| c["id"] == id).unwrap().clone();
    assert_eq!(find(&a_id)["has_artefacts"], true, "emitting chat must flag true");
    assert_eq!(find(&b_id)["has_artefacts"], false, "silent chat must stay false");
}

#[tokio::test]
async fn artefact_deep_link_is_shareable() {
    // Two linked contracts the UI relies on to make artefact URLs
    // "go straight to the artefact":
    //   1. Naked `/artefacts/<id>` 302-redirects to the SPA deep-link
    //      `/#/artefacts/<id>` so a link pasted into chat/docs opens
    //      the reader, not a download.
    //   2. The raw bytes endpoint surfaces the owning chat id via an
    //      `X-Dyson-Chat-Id` header so a cold deep-link can restore
    //      the sidebar without a second round-trip.
    let r = rig().await;

    let created = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "permalinks" }),
    ).await).await;
    let id = created["id"].as_str().unwrap().to_string();

    let mut sse = request(
        &format!("{}/api/conversations/{}/events", r.base, id),
        Method::GET,
        None,
    ).await;
    assert_eq!(sse.status(), StatusCode::OK);
    test_helpers::wait_for_sse_subscriber(r.state.clone(), &id).await;

    let artefact = dyson::message::Artefact::markdown(
        dyson::message::ArtefactKind::SecurityReview,
        "Report",
        "body",
    );
    test_helpers::emit_agent_artefact(r.state.clone(), &id, artefact)
        .await
        .expect("emit artefact");
    let evt = read_sse_event(&mut sse).await;
    let spa_url = evt["url"].as_str().unwrap().to_string();
    let art_id = spa_url.trim_start_matches("/#/artefacts/").to_string();

    // 1. Naked `/artefacts/<id>` 302s to the SPA deep-link.
    let redir = get(&format!("{}/artefacts/{}", r.base, art_id)).await;
    assert_eq!(redir.status(), StatusCode::FOUND);
    let loc = redir.headers().get("location").expect("location header");
    assert_eq!(loc.to_str().unwrap(), format!("/#/artefacts/{art_id}"));

    // 2. The raw endpoint carries the chat id.
    let body = get(&format!("{}/api/artefacts/{}", r.base, art_id)).await;
    assert_eq!(body.status(), StatusCode::OK);
    let chat_hdr = body
        .headers()
        .get("x-dyson-chat-id")
        .expect("X-Dyson-Chat-Id header must be present");
    assert_eq!(chat_hdr.to_str().unwrap(), id);
}

#[tokio::test]
async fn emitted_images_survive_refresh_via_artefacts() {
    // Regression: images emitted via Output::send_file must survive a
    // browser refresh AND a controller restart.  The chat-scroll chip
    // is purely a live-stream artefact (not in message history), so
    // the Artefacts tab is the durable surface — it must list every
    // image ever emitted for this chat, even on a fresh HttpState.
    let r = rig().await;

    let created = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "images-persist" }),
    ).await).await;
    let id = created["id"].as_str().unwrap().to_string();

    // Subscribe before emission so the broadcast has a receiver.
    let mut sse = request(
        &format!("{}/api/conversations/{}/events", r.base, id),
        Method::GET,
        None,
    ).await;
    assert_eq!(sse.status(), StatusCode::OK);
    test_helpers::wait_for_sse_subscriber(r.state.clone(), &id).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let png_path = dir.path().join("generated.png");
    std::fs::write(&png_path, PNG_1X1).expect("write png");
    test_helpers::emit_agent_file(r.state.clone(), &id, &png_path)
        .await
        .expect("emit png");

    // Drain the `file` + `artefact` events from the stream.
    let evt_file = read_sse_event(&mut sse).await;
    assert_eq!(evt_file["type"], "file");
    let file_url = evt_file["url"].as_str().unwrap().to_string();
    let evt_art = read_sse_event(&mut sse).await;
    assert_eq!(evt_art["type"], "artefact");
    assert_eq!(evt_art["kind"], "image");

    // Simulate a refresh: re-fetch the artefact list via the API the
    // frontend uses on chat load.  The image MUST be there.
    let list = body_json(get(&format!(
        "{}/api/conversations/{}/artefacts",
        r.base, id,
    )).await).await;
    let arr = list.as_array().expect("list array");
    assert_eq!(arr.len(), 1, "image artefact missing on refresh: {list}");
    assert_eq!(arr[0]["kind"], "image");
    let art_id = arr[0]["id"].as_str().unwrap().to_string();

    // Simulate a controller restart: build a fresh HttpState pointed
    // at the same chat dir.  The image body must rehydrate from disk.
    let mut rebuilt_settings = Settings::default();
    rebuilt_settings.chat_history = ChatHistoryConfig {
        backend: "disk".into(),
        connection_string: Credential::new(
            r.chat_dir.path().to_string_lossy().into_owned(),
        ),
    };
    rebuilt_settings.workspace.connection_string =
        Credential::new(r.workspace_dir.path().to_string_lossy().into_owned());
    let mut providers = std::collections::HashMap::new();
    providers.insert(
        "default".to_string(),
        ProviderConfig {
            provider_type: LlmProvider::OpenRouter,
            api_key: Credential::new("sk-test".into()),
            base_url: None,
            models: vec!["qwen/qwen3.6-plus".to_string()],
        },
    );
    rebuilt_settings.providers = providers;
    rebuilt_settings.agent.provider = LlmProvider::OpenRouter;
    rebuilt_settings.agent.model = "qwen/qwen3.6-plus".into();

    let registry2 = Arc::new(ClientRegistry::new(&rebuilt_settings, None));
    let history2: Arc<dyn ChatHistory> = Arc::new(
        DiskChatHistory::new(r.chat_dir.path().to_path_buf()).expect("disk history"),
    );
    let feedback2 = Arc::new(FeedbackStore::new(r.chat_dir.path().to_path_buf()));
    let state2 = test_helpers::build_state(
        rebuilt_settings,
        registry2,
        None,
        Some(history2),
        Some(feedback2),
        Arc::new(DangerousNoAuth),
    );

    // The rebuilt store must have the artefact entry indexed AND the
    // file bytes must be retrievable from the new controller's own
    // /api/files/<id> (which falls through to disk for IDs the
    // in-memory FileStore doesn't have yet).
    assert!(
        state2.artefacts_for_test(&art_id).is_some(),
        "image artefact must rehydrate from disk",
    );
    let file_id = file_url.trim_start_matches("/api/files/");
    assert!(
        state2.file_bytes_for_test(file_id).is_some(),
        "image file bytes must be reachable from fresh state via disk fallback",
    );
}

#[tokio::test]
async fn image_artefact_stamps_tool_use_id_for_panel_rehydration() {
    // Regression: when image_generate emits its file during an active
    // tool call, the ArtefactEntry must record the originating
    // tool_use_id and the chat-load DTO must expose it.  The frontend
    // uses this to flip the `image_generate` tool panel from its
    // default text fallback into image-kind on refresh — without it
    // the refreshed page shows the tool as "ok (33037ms) / Generated
    // 1 image(s)..." and the image never reaches the panel.
    let r = rig().await;

    let created = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "tool_use_id" }),
    ).await).await;
    let id = created["id"].as_str().unwrap().to_string();

    let mut sse = request(
        &format!("{}/api/conversations/{}/events", r.base, id),
        Method::GET,
        None,
    ).await;
    assert_eq!(sse.status(), StatusCode::OK);
    test_helpers::wait_for_sse_subscriber(r.state.clone(), &id).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let png_path = dir.path().join("gen.png");
    std::fs::write(&png_path, PNG_1X1).expect("write png");

    // Emit the file as if during an active `image_generate` tool call —
    // exactly what SseOutput does after tool_use_start.
    test_helpers::emit_agent_file_for_tool(
        r.state.clone(),
        &id,
        &png_path,
        Some("tool_use_image_generate_42"),
    )
    .await
    .expect("emit png");

    // Drain the `file` + `artefact` events.
    let _ = read_sse_event(&mut sse).await;
    let art_evt = read_sse_event(&mut sse).await;
    assert_eq!(art_evt["type"], "artefact");
    let art_id = art_evt["url"]
        .as_str()
        .unwrap()
        .trim_start_matches("/#/artefacts/")
        .to_string();

    // The entry must carry the tool_use_id for the frontend to pair
    // it with the right tool panel.
    assert_eq!(
        r.state.artefact_tool_use_id_for_test(&art_id).as_deref(),
        Some("tool_use_image_generate_42"),
        "ArtefactEntry.tool_use_id must be stamped at emission",
    );

    // The chat-load DTO must expose it as well.
    let convo = body_json(
        get(&format!("{}/api/conversations/{}", r.base, id)).await,
    ).await;
    let messages = convo["messages"].as_array().expect("messages");
    let artefact_chip = messages
        .last()
        .unwrap()["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "artefact")
        .expect("artefact chip");
    assert_eq!(
        artefact_chip["tool_use_id"], "tool_use_image_generate_42",
        "chat-load DTO must expose tool_use_id: {artefact_chip}",
    );
    assert_eq!(artefact_chip["kind"], "image");
    // metadata carries file_url so the reader / tool panel can render
    // without a second round-trip.
    assert!(
        artefact_chip["metadata"]["file_url"]
            .as_str()
            .is_some_and(|s| s.starts_with("/api/files/")),
        "metadata.file_url missing: {artefact_chip}",
    );
}

#[tokio::test]
async fn chat_reload_shows_emitted_images_in_transcript() {
    // Regression: on browser refresh the chat-scroll must still show
    // agent-emitted images as artefact chips.  Files are side-channel
    // (not in message history), so the /api/conversations/:id response
    // must synthesise an artefact-chip turn from the ArtefactStore.
    let r = rig().await;

    let created = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "chat-reload" }),
    ).await).await;
    let id = created["id"].as_str().unwrap().to_string();

    let mut sse = request(
        &format!("{}/api/conversations/{}/events", r.base, id),
        Method::GET,
        None,
    ).await;
    assert_eq!(sse.status(), StatusCode::OK);
    test_helpers::wait_for_sse_subscriber(r.state.clone(), &id).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let png_path = dir.path().join("hello.png");
    std::fs::write(&png_path, PNG_1X1).expect("write png");
    test_helpers::emit_agent_file(r.state.clone(), &id, &png_path)
        .await
        .expect("emit png");

    // Drain the two live events so the broadcast doesn't back up.
    let _ = read_sse_event(&mut sse).await;
    let _ = read_sse_event(&mut sse).await;

    // Now simulate a refresh — fetch the conversation fresh.  The last
    // turn of the transcript must carry an artefact chip pointing at
    // the image.
    let convo = body_json(
        get(&format!("{}/api/conversations/{}", r.base, id)).await,
    ).await;
    let messages = convo["messages"].as_array().expect("messages array");
    assert!(!messages.is_empty(), "transcript must not be empty on reload");
    let last = messages.last().unwrap();
    let blocks = last["blocks"].as_array().expect("blocks array");
    let artefact_chip = blocks
        .iter()
        .find(|b| b["type"] == "artefact")
        .expect("refresh transcript must contain an artefact chip");
    assert_eq!(artefact_chip["kind"], "image");
    assert_eq!(artefact_chip["title"], "hello.png");
    let url = artefact_chip["url"].as_str().expect("url on chip");
    assert!(url.starts_with("/#/artefacts/"), "chip url: {url}");
}

