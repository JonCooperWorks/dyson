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
use dyson::controller::http::test_helpers;
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
    let handle = tokio::spawn(test_helpers::serve(state, listener));

    // Tiny settle so the spawn is in the accept loop before the first
    // request races it.
    tokio::time::sleep(Duration::from_millis(20)).await;

    Rig {
        base,
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

    // No fabricated fields — assert the contract the bridge depends on.
    let keys: Vec<&str> = only.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["id", "live", "title"]);
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

    // Confirm it landed in the SAME file the Telegram controller writes
    // — chat_id_feedback.json under chat_history.connection_string.
    let path = r.chat_dir.path().join(format!("{id}_feedback.json"));
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
