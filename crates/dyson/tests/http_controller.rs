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

use dyson::auth::{Auth, Credential, DangerousNoAuth, HashedBearerAuth};
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
    // If the caller supplied an override Host header, drop the default
    // so dispatch sees only one — DNS-rebinding tests need this.
    let caller_set_host = extra_headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("host"));
    let mut builder = Request::builder().method(method).uri(path_q);
    if !caller_set_host {
        builder = builder.header(hyper::header::HOST, authority);
    }
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
async fn providers_list_reflects_hot_reloaded_models() {
    // Regression: a model added to dyson.json while the controller
    // was running never surfaced in `GET /api/providers` because
    // state.settings was a frozen clone from startup.  Fix wraps
    // settings in an RwLock and a background task swaps it on a
    // dyson.json change.  This test skips the filesystem side and
    // drives the swap directly.
    let r = rig().await;

    let before = body_json(get(&format!("{}/api/providers", r.base)).await).await;
    let initial_models: Vec<&str> = before[0]["models"].as_array().unwrap()
        .iter().map(|v| v.as_str().unwrap()).collect();
    assert!(!initial_models.contains(&"anthropic/claude-opus-4-7"),
        "pre-condition: new model must not already be listed: {initial_models:?}");

    // Build a fresh Settings with an extra model appended to the
    // existing provider — simulates the operator editing dyson.json.
    let mut new_settings = r.state.settings_snapshot();
    new_settings
        .providers
        .get_mut("default")
        .expect("default provider")
        .models
        .push("anthropic/claude-opus-4-7".to_string());
    r.state.replace_settings_for_test(new_settings);

    let after = body_json(get(&format!("{}/api/providers", r.base)).await).await;
    let updated_models: Vec<&str> = after[0]["models"].as_array().unwrap()
        .iter().map(|v| v.as_str().unwrap()).collect();
    assert!(
        updated_models.contains(&"anthropic/claude-opus-4-7"),
        "hot-reloaded model must appear in /api/providers: {updated_models:?}",
    );
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
    // The Vite bundle hashes asset filenames, so the test fetches `/`
    // and discovers the real paths from the injected <script>/<link>
    // tags rather than hardcoding names that would drift on rebuild.
    let r = rig().await;
    let resp = get(&format!("{}/", r.base)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("text/html"));
    let html = body_string(resp).await;

    let js = find_asset_href(&html, ".js").expect("index.html must link a JS chunk");
    let resp = get(&format!("{}{}", r.base, js)).await;
    assert_eq!(resp.status(), StatusCode::OK, "GET {js}");
    let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("application/javascript"), "js content-type = {ct}");
    assert!(!body_string(resp).await.is_empty(), "empty JS chunk");

    // Vite inlines small CSS into a <style> tag in index.html and only
    // emits a separate `.css` chunk past a size threshold.  Either is
    // fine — accept whichever shape the bundle produced.
    if let Some(css) = find_asset_href(&html, ".css") {
        let resp = get(&format!("{}{}", r.base, css)).await;
        assert_eq!(resp.status(), StatusCode::OK, "GET {css}");
        let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.starts_with("text/css"), "css content-type = {ct}");
    } else {
        assert!(html.contains("<style"), "no .css chunk and no inline <style> in index.html");
    }
}

fn find_asset_href(html: &str, suffix: &str) -> Option<String> {
    // Simple scanner — the injected tags always use double quotes and
    // root-relative paths (`/assets/...`).  Good enough for the test.
    for needle in ["src=\"", "href=\""] {
        let mut rest = html;
        while let Some(pos) = rest.find(needle) {
            let after = &rest[pos + needle.len()..];
            if let Some(end) = after.find('"') {
                let candidate = &after[..end];
                if candidate.starts_with('/') && candidate.ends_with(suffix) {
                    return Some(candidate.to_string());
                }
                rest = &after[end..];
            } else {
                break;
            }
        }
    }
    None
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
async fn post_model_surfaces_choice_in_providers_listing() {
    // Regression: the bug was that web's model switch evaporated the
    // next time an agent was built — `/api/providers` reflected the
    // startup setting, not the operator's most recent choice.  After
    // `POST /api/model`, the active provider/model in the listing
    // must match what was just set.
    let r = rig().await;

    // Pick a non-default model from the provider's configured set.
    let before = body_json(get(&format!("{}/api/providers", r.base)).await).await;
    let provider_id = before.as_array().unwrap()[0]["id"].as_str().unwrap().to_string();
    let models = before.as_array().unwrap()[0]["models"].as_array().unwrap().clone();
    assert!(models.len() >= 2, "fixture provider must list >=2 models for this test: {models:?}");
    let current = before.as_array().unwrap()[0]["active_model"].as_str().unwrap().to_string();
    let switch_to = models
        .iter()
        .filter_map(|v| v.as_str())
        .find(|m| *m != current)
        .expect("another model exists")
        .to_string();

    let resp = body_json(post_json(
        &format!("{}/api/model", r.base),
        &serde_json::json!({ "provider": provider_id, "model": switch_to }),
    ).await).await;
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["model"], switch_to);

    // Providers listing must now report the switched model as active —
    // this is what the web UI reads on each poll and what new chats
    // get wired to via the runtime override applied in `post_turn`.
    let after = body_json(get(&format!("{}/api/providers", r.base)).await).await;
    let active = &after.as_array().unwrap()[0];
    assert_eq!(active["active"], true);
    assert_eq!(active["active_model"], switch_to, "providers listing must reflect the switch");
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
    // can't probe outside the embedded asset table.  Returns 404, not
    // 403, on purpose — same shape as a missing asset.
    let r = rig().await;
    for evil in [
        "/../../../../etc/passwd",
        "/assets/../../etc/hosts",
        "/assets/..%2Findex.html",
        // Fully-encoded variants — the check must run after url_decode,
        // otherwise %2e%2e%2f sails past a literal `..` string-contains.
        "/%2e%2e%2f%2e%2e%2fetc/passwd",
        "/assets/%2e%2e/%2e%2e/etc/hosts",
        "/%2E%2E%2Fetc/hosts",
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
        settings, registry, Some(history), Some(feedback),
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
async fn root_path_serves_index_html() {
    // GET / must serve the Vite-built index.html, not redirect or 404.
    let r = rig().await;
    let resp = get(&format!("{}/", r.base)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("text/html"));
    let html = body_string(resp).await;
    assert!(html.contains("id=\"root\""));
    assert!(
        find_asset_href(&html, ".js").is_some(),
        "index.html must link a JS chunk under /assets/",
    );
}

// ---------------------------------------------------------------------------
// Auth — DangerousNoAuth is exercised implicitly by every test above; here
// we lock in the Bearer-protected path and the static-shell exemption.
// ---------------------------------------------------------------------------

fn hashed_bearer_for_test(plaintext: &str) -> Arc<HashedBearerAuth> {
    let phc = HashedBearerAuth::hash(plaintext).expect("argon2 hash");
    Arc::new(HashedBearerAuth::from_phc(phc).expect("phc parse"))
}

#[tokio::test]
async fn bearer_auth_rejects_unauthenticated_api_request() {
    let auth = hashed_bearer_for_test("s3cret");
    let r = rig_with_auth(auth).await;
    let resp = get(&format!("{}/api/conversations", r.base)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "unauthorized");
}

#[tokio::test]
async fn bearer_auth_rejects_wrong_token() {
    let auth = hashed_bearer_for_test("correct");
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
    let auth = hashed_bearer_for_test("right-token");
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
    // so `/` and `/assets/*` are exempt.  Without this the UI would
    // 401 on the very first GET /.
    let auth = hashed_bearer_for_test("s3cret");
    let r = rig_with_auth(auth).await;
    let html_resp = get(&format!("{}/", r.base)).await;
    assert_eq!(html_resp.status(), StatusCode::OK, "GET / must be exempt");
    let html = body_string(html_resp).await;
    let js = find_asset_href(&html, ".js").expect("index.html must link a JS chunk");
    let resp = get(&format!("{}{}", r.base, js)).await;
    assert_eq!(resp.status(), StatusCode::OK, "GET {js} must be exempt");
    if let Some(css) = find_asset_href(&html, ".css") {
        let resp = get(&format!("{}{}", r.base, css)).await;
        assert_eq!(resp.status(), StatusCode::OK, "GET {css} must be exempt");
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
async fn http_controller_accepts_bearer_config_with_argon2_hash() {
    use dyson::config::ControllerConfig;
    use dyson::controller::http::HttpController;

    let phc = HashedBearerAuth::hash("abc123").expect("hash");
    let cfg = ControllerConfig {
        controller_type: "http".into(),
        config: serde_json::json!({
            "bind": "127.0.0.1:0",
            "auth": { "type": "bearer", "hash": phc },
        }),
    };
    assert!(HttpController::from_config(&cfg).is_some());
}

#[tokio::test]
async fn http_controller_rejects_bearer_with_empty_hash() {
    use dyson::config::ControllerConfig;
    use dyson::controller::http::HttpController;

    let cfg = ControllerConfig {
        controller_type: "http".into(),
        config: serde_json::json!({
            "bind": "127.0.0.1:0",
            "auth": { "type": "bearer", "hash": "" },
        }),
    };
    assert!(HttpController::from_config(&cfg).is_none());
}

#[tokio::test]
async fn http_controller_rejects_bearer_with_non_phc_hash() {
    // A plaintext token slipped into the `hash` field must be rejected:
    // we promised operators we'd argon2-verify, not byte-compare.
    use dyson::config::ControllerConfig;
    use dyson::controller::http::HttpController;

    let cfg = ControllerConfig {
        controller_type: "http".into(),
        config: serde_json::json!({
            "bind": "127.0.0.1:0",
            "auth": { "type": "bearer", "hash": "plaintext-token" },
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
    // SSE records are multiple `field: value\n` lines terminated by
    // `\n\n`.  After the last-event-id work, every record carries
    // `id: <n>\ndata: <json>\n\n`, so the reader scans the record's
    // lines and pulls the `data:` payload regardless of whether
    // `id:` came first.
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
            for line in record.lines() {
                if let Some(payload) = line.strip_prefix("data: ") {
                    return serde_json::from_str(payload).expect("sse json");
                }
            }
            // Comment-only frames (": lag\n\n") or id-only records →
            // keep reading for the real event.
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
// Browser artefact sink — the bridge that makes Telegram's `send_file`
// land in the web UI's Artefacts tab for the same chat id.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn browser_artefact_sink_publishes_telegram_file_as_artefact() {
    // Simulates the telegram → browser path: a file delivered through
    // the Telegram controller should show up in the HTTP controller's
    // Artefacts tab for the matching chat id, with both the served
    // bytes and the per-chat listing reachable from the browser.
    let r = rig().await;

    // Telegram chat ids are bare numeric strings (see
    // `controller::http::source_for_chat_id`).  Use one here so the
    // chat is indistinguishable from a real Telegram chat to the rest
    // of the controller.
    let chat_id = "2102424765".to_string();

    // Hydrate a ChatHandle for this chat id — the sink only
    // broadcasts live SSE when one exists.  Listing conversations is
    // the cheap, idempotent way to trigger hydration.
    let _ = get(&format!("{}/api/conversations", r.base)).await;
    // Nothing on disk yet, so prime the map directly by listing from
    // an explicit POST wouldn't work (it would mint `c-NNNN`).  The
    // sink still persists to disk regardless — force a handle to
    // exist by writing a transcript file the controller can discover.
    let chat_sub = r.chat_dir.path().join(&chat_id);
    std::fs::create_dir_all(&chat_sub).expect("mkdir chat");
    std::fs::write(
        chat_sub.join("transcript.json"),
        r#"{"version":1,"messages":[]}"#,
    )
    .expect("write transcript");
    let _ = body_json(get(&format!("{}/api/conversations", r.base)).await).await;

    let mut sse = request(
        &format!("{}/api/conversations/{}/events", r.base, chat_id),
        Method::GET,
        None,
    ).await;
    assert_eq!(sse.status(), StatusCode::OK);
    test_helpers::wait_for_sse_subscriber(r.state.clone(), &chat_id).await;

    // Publish a PDF through the sink — the same path Telegram's
    // `send_file` takes when an HTTP controller is also running.
    let dir = tempfile::tempdir().expect("tempdir");
    let pdf_path = dir.path().join("invoice.pdf");
    let pdf_bytes = b"%PDF-1.4\n% telegram upload\n";
    std::fs::write(&pdf_path, pdf_bytes).expect("write pdf");
    let (_file_id, art_id) =
        test_helpers::publish_file_as_artefact_for_test(r.state.clone(), &chat_id, &pdf_path)
            .expect("sink must succeed on a readable file");

    // SSE broadcast: file event then artefact event, in that order.
    let file_evt = read_sse_event(&mut sse).await;
    assert_eq!(file_evt["type"], "file", "first event: {file_evt}");
    assert_eq!(file_evt["name"], "invoice.pdf");
    assert_eq!(file_evt["mime_type"], "application/pdf");
    assert_eq!(file_evt["inline_image"], false);
    let file_url = file_evt["url"].as_str().expect("file url").to_string();

    let art_evt = read_sse_event(&mut sse).await;
    assert_eq!(art_evt["type"], "artefact", "second event: {art_evt}");
    assert_eq!(art_evt["title"], "invoice.pdf");
    assert_eq!(art_evt["kind"], "other", "non-images surface as kind=other");
    assert_eq!(art_evt["bytes"], pdf_bytes.len());
    let art_url = art_evt["url"].as_str().expect("artefact url").to_string();
    assert!(
        art_url.starts_with("/#/artefacts/"),
        "artefact url is an SPA deep-link: {art_url}",
    );

    // The served file endpoint returns the bytes verbatim.
    let resp = get(&format!("{}{}", r.base, file_url)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.expect("collect").to_bytes();
    assert_eq!(&body[..], pdf_bytes);

    // Per-chat artefact listing must include the new entry.
    let list = body_json(get(&format!(
        "{}/api/conversations/{}/artefacts",
        r.base, chat_id,
    )).await).await;
    let arr = list.as_array().expect("artefact list is array");
    assert!(
        arr.iter().any(|a| a["id"] == art_id.as_str()),
        "sink artefact must appear in per-chat list: {list}",
    );
}

#[tokio::test]
async fn browser_artefact_sink_missing_file_is_noop() {
    // Telegram hands us a path for a file that's already been cleaned
    // up — the sink must decline silently rather than poison the
    // store with a broken artefact entry.
    let r = rig().await;
    let chat_id = "999".to_string();

    let dir = tempfile::tempdir().expect("tempdir");
    let gone = dir.path().join("ghost.pdf");
    assert!(!gone.exists());

    let result =
        test_helpers::publish_file_as_artefact_for_test(r.state.clone(), &chat_id, &gone);
    assert!(result.is_none(), "sink must return None when the file is gone");

    let list = body_json(get(&format!(
        "{}/api/conversations/{}/artefacts",
        r.base, chat_id,
    )).await).await;
    let arr = list.as_array().expect("artefact list is array");
    assert!(arr.is_empty(), "failed publish must not surface anything: {list}");
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
async fn chat_reload_rehydrates_user_uploaded_images_as_file_blocks() {
    // Regression: a chat that once carried an image-bearing user turn
    // reloaded with `{ type: "text", text: "[non-text content]" }`
    // placeholders, which the UI rendered as literal "[non-text
    // content]" where the screenshot used to be.  After this fix the
    // same turn comes back as a `{ type: "file", mime, url, inline_image }`
    // block so `FileBlock` renders the image inline with the data URL.
    let r = rig().await;

    let created = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "image recall" }),
    ).await).await;
    let id = created["id"].as_str().unwrap().to_string();

    // Seed a user turn with an Image block directly — simulates what
    // `run_with_attachments` writes to disk after a Telegram photo
    // or a web composer upload.
    {
        use dyson::message::{ContentBlock, Message, Role};
        let history = r.state.history_for_test().expect("disk history");
        let msgs = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text { text: "what's in this image?".into() },
                ContentBlock::Image {
                    data: "R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw==".into(),
                    media_type: "image/gif".into(),
                },
            ],
        }];
        history.save(&id, &msgs).expect("save seeded transcript");
    }

    let convo = body_json(
        get(&format!("{}/api/conversations/{}", r.base, id)).await,
    ).await;
    let messages = convo["messages"].as_array().expect("messages");
    let user = messages.iter().find(|m| m["role"] == "user").expect("user msg");
    let blocks = user["blocks"].as_array().expect("blocks");
    let file_block = blocks
        .iter()
        .find(|b| b["type"] == "file")
        .expect("image must rehydrate as file block, not '[non-text content]' placeholder");
    assert_eq!(file_block["mime"], "image/gif");
    assert_eq!(file_block["inline_image"], true);
    let url = file_block["url"].as_str().expect("file block url");
    assert!(
        url.starts_with("data:image/gif;base64,"),
        "url must be a self-contained data URL: {url}",
    );
    // And the old "[non-text content]" text marker must not be
    // present — that's the exact string the UI was rendering before.
    for block in blocks {
        if let Some(t) = block["text"].as_str() {
            assert_ne!(t, "[non-text content]", "legacy placeholder must be gone: {blocks:?}");
        }
    }
}

#[tokio::test]
async fn export_conversation_returns_sharegpt_json() {
    // The web UI's download button hits this endpoint.  Regression:
    // a conversation with zero messages 404s cleanly (nothing to
    // export), a populated chat returns valid ShareGPT JSON with the
    // chat id stamped.  Replaces the old `/export` slash command
    // which relied on workspace paths that don't exist everywhere.
    let r = rig().await;

    // Empty chat → 404 (nothing to export yet).
    let a = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "empty" }),
    ).await).await;
    let a_id = a["id"].as_str().unwrap().to_string();
    let empty = get(&format!("{}/api/conversations/{}/export", r.base, a_id)).await;
    assert_eq!(empty.status(), StatusCode::NOT_FOUND);

    // Populated chat → 200 with a ShareGPT array and attachment
    // disposition so the browser prompts a save.
    let b = body_json(post_json(
        &format!("{}/api/conversations", r.base),
        &serde_json::json!({ "title": "with turns" }),
    ).await).await;
    let b_id = b["id"].as_str().unwrap().to_string();
    test_helpers::seed_transcript(
        r.state.clone(),
        &b_id,
        &[
            ("user", "hello"),
            ("assistant", "hi there"),
        ],
    )
    .await
    .expect("seed");

    let resp = get(&format!("{}/api/conversations/{}/export", r.base, b_id)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cd = resp
        .headers()
        .get("content-disposition")
        .expect("CD header must be present")
        .to_str()
        .unwrap()
        .to_string();
    assert!(cd.contains("attachment"), "CD must trigger save dialog: {cd}");
    assert!(cd.contains(&b_id), "filename stamps chat id: {cd}");
    let body = body_json(resp).await;
    let arr = body.as_array().expect("sharegpt is a JSON array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], b_id);
    let turns = arr[0]["conversations"].as_array().expect("turns array");
    assert!(!turns.is_empty(), "turns must not be empty: {body}");
    assert_eq!(turns[0]["from"], "human");
    assert_eq!(turns[0]["value"], "hello");
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
async fn artefact_id_rejects_url_encoded_traversal() {
    // Before the gate landed, `/api/artefacts/<id>` url-decoded the id
    // and handed it to ArtefactStore::load_from_disk, which joined it
    // into `sub.join(format!("{id}.meta.json"))`.  A decoded `../`
    // would have escaped the store dir.  `/artefacts/<id>` had the
    // analogous bypass — its `!id.contains('/')` check ran before
    // url_decode so `%2F` slipped through and landed in the Location
    // header.  Both must 404 now regardless of encoding.
    let r = rig().await;
    for evil in [
        "..%2F..%2Fetc%2Fpasswd",
        "%2e%2e%2fsecret",
        "..%2fx",
        "a0%2F..",
        "a0/b1", // raw `/` — structurally bogus for a single id
    ] {
        let api = get(&format!("{}/api/artefacts/{}", r.base, evil)).await;
        assert_eq!(
            api.status(),
            StatusCode::NOT_FOUND,
            "/api/artefacts/{} should 404, got {}",
            evil,
            api.status(),
        );
        let redir = get(&format!("{}/artefacts/{}", r.base, evil)).await;
        assert_eq!(
            redir.status(),
            StatusCode::NOT_FOUND,
            "/artefacts/{} should 404, got {}",
            evil,
            redir.status(),
        );
    }
    for evil in ["..%2Ffoo", "%2e%2e"] {
        let resp = get(&format!("{}/api/files/{}", r.base, evil)).await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "/api/files/{} should 404, got {}",
            evil,
            resp.status(),
        );
    }
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

// ---------------------------------------------------------------------------
// Activity registry + /api/activity
//
// The Activity tab's Subagents lane needs live state AND persistence.
// Three tests:
//   (1) round-trip — start/finish an activity entry and confirm
//       `/api/activity` surfaces it with the right shape.
//   (2) chat filter — `?chat=<id>` returns only one chat's entries.
//   (3) restart survival — entries written by one HttpState survive
//       a drop + re-init against the same data_dir.
//
// We go direct via `test_helpers::activity_handle` rather than driving
// a real subagent (which would require a full LLM mock wired all the
// way through `build_agent`) — the tool-side emission is already
// covered by `skill::subagent::tests::orchestrator_emits_artefact_*`
// and the aim here is the HTTP contract.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn activity_endpoint_returns_running_then_finished_entries() {
    let r = rig().await;
    // Mint a chat so the activity entry has a valid chat_id context
    // (not strictly required by the registry but mirrors live usage).
    let chat = create_chat(&r, "activity test").await;

    let handle = dyson::controller::http::test_helpers::activity_handle(
        &r.state,
        &chat,
    );

    // 1. Start — endpoint shows Running.
    let tok = handle.start(
        dyson::controller::LANE_SUBAGENT,
        "security_engineer",
        "review crates/dyson",
    );
    let running = body_json(get(&format!("{}/api/activity", r.base)).await).await;
    let lanes = running["lanes"].as_array().expect("lanes array");
    assert_eq!(lanes.len(), 1, "one entry expected while running");
    assert_eq!(lanes[0]["lane"], "subagent");
    assert_eq!(lanes[0]["name"], "security_engineer");
    assert_eq!(lanes[0]["status"], "running");
    assert_eq!(lanes[0]["chat_id"], chat);
    assert!(
        lanes[0]["note"].as_str().unwrap().contains("review crates/dyson"),
        "note should carry truncated task input: {:?}", lanes[0]["note"],
    );

    // 2. Finish Ok — status flips, finished_at populated.
    tok.finish(dyson::controller::ActivityStatus::Ok, Some("42s"));
    let finished = body_json(get(&format!("{}/api/activity", r.base)).await).await;
    let lanes = finished["lanes"].as_array().expect("lanes array");
    assert_eq!(lanes[0]["status"], "ok");
    assert!(lanes[0]["finished_at"].is_number());
    assert!(
        lanes[0]["note"].as_str().unwrap().contains("42s"),
        "note suffix should have been appended: {:?}", lanes[0]["note"],
    );
}

#[tokio::test]
async fn activity_endpoint_filters_by_chat() {
    let r = rig().await;
    let chat_a = create_chat(&r, "chat a").await;
    let chat_b = create_chat(&r, "chat b").await;

    let h_a = dyson::controller::http::test_helpers::activity_handle(&r.state, &chat_a);
    let h_b = dyson::controller::http::test_helpers::activity_handle(&r.state, &chat_b);
    let _t1 = h_a.start(dyson::controller::LANE_SUBAGENT, "se", "a1");
    let _t2 = h_b.start(dyson::controller::LANE_SUBAGENT, "se", "b1");
    let _t3 = h_b.start(dyson::controller::LANE_SUBAGENT, "se", "b2");

    let all = body_json(get(&format!("{}/api/activity", r.base)).await).await;
    assert_eq!(all["lanes"].as_array().unwrap().len(), 3);

    let scoped = body_json(
        get(&format!("{}/api/activity?chat={}", r.base, chat_b)).await,
    ).await;
    let scoped_lanes = scoped["lanes"].as_array().expect("lanes array");
    assert_eq!(scoped_lanes.len(), 2, "only chat_b entries");
    for lane in scoped_lanes {
        assert_eq!(lane["chat_id"], chat_b);
    }
}

#[tokio::test]
async fn activity_entries_survive_controller_restart() {
    // Spin rig A, record an entry, drop it, spin rig B against the
    // same data_dir, hit /api/activity, confirm the entry is present.
    let chat_dir = tempfile::tempdir().expect("chat tempdir");
    let workspace_dir = tempfile::tempdir().expect("workspace tempdir");
    let chat_id = "c-0042";
    // Pre-seed the chat dir so the registry's disk path exists.
    std::fs::create_dir_all(chat_dir.path().join(chat_id)).unwrap();

    // --- rig A: write one completed entry ---
    {
        let r = rig_pointing_at(&chat_dir, &workspace_dir).await;
        let h = dyson::controller::http::test_helpers::activity_handle(&r.state, chat_id);
        let tok = h.start(
            dyson::controller::LANE_SUBAGENT,
            "security_engineer",
            "persisted review",
        );
        tok.finish(dyson::controller::ActivityStatus::Ok, Some("17s"));
        // Drop rig A (chat_dir kept alive at outer scope).
    }

    // --- rig B: fresh HttpState pointing at the same disk ---
    let r = rig_pointing_at(&chat_dir, &workspace_dir).await;
    let j = body_json(
        get(&format!("{}/api/activity?chat={}", r.base, chat_id)).await,
    ).await;
    let lanes = j["lanes"].as_array().expect("lanes array");
    assert_eq!(lanes.len(), 1, "entry should survive restart");
    assert_eq!(lanes[0]["name"], "security_engineer");
    assert_eq!(lanes[0]["status"], "ok");
    assert!(
        lanes[0]["note"].as_str().unwrap().contains("17s"),
        "note suffix should round-trip through disk: {:?}", lanes[0]["note"],
    );
}

/// Helper that mints a chat and returns its id.  The `/api/activity`
/// endpoint doesn't strictly require a chat to exist (the registry
/// keys by arbitrary string) but the real HTTP flow always has one.
async fn create_chat(r: &Rig, title: &str) -> String {
    let body = serde_json::json!({ "title": title });
    let resp = post_json(&format!("{}/api/conversations", r.base), &body).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    j["id"].as_str().expect("chat id").to_string()
}

/// Build a rig with a custom `AuthMode`.  Lets the OIDC / Bearer
/// /api/auth/config + WWW-Authenticate tests pin the SPA-facing
/// summary the controller reports without needing a real IdP behind
/// it (the validation gate uses whatever `auth: Arc<dyn Auth>` the
/// caller supplies, independent of the AuthMode).
async fn rig_with_auth_and_mode(
    auth: Arc<dyn Auth>,
    auth_mode: test_helpers::AuthMode,
) -> Rig {
    let chat_dir = tempfile::tempdir().expect("chat tempdir");
    let workspace_dir = tempfile::tempdir().expect("workspace tempdir");

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
    let state = test_helpers::build_state_with_auth_mode(
        settings,
        registry,
        Some(history),
        Some(feedback),
        auth,
        auth_mode,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let base = format!("http://{}", addr);
    let handle = tokio::spawn(test_helpers::serve(state.clone(), listener));
    tokio::time::sleep(Duration::from_millis(20)).await;
    Rig {
        base,
        state,
        chat_dir,
        workspace_dir,
        _handle: handle,
    }
}

/// Build a Rig pointing at specific tempdirs — used by the
/// restart-survival test so rig A and rig B share the same disk.
async fn rig_pointing_at(
    chat_dir: &tempfile::TempDir,
    workspace_dir: &tempfile::TempDir,
) -> Rig {
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
        Some(history),
        Some(feedback),
        Arc::new(DangerousNoAuth),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let base = format!("http://{}", addr);
    let handle = tokio::spawn(test_helpers::serve(state.clone(), listener));
    tokio::time::sleep(Duration::from_millis(20)).await;
    Rig {
        base,
        state,
        // Can't move/clone the TempDirs here — we don't own them.  The
        // restart-survival test keeps the originals alive in its own
        // scope so the disk stays valid for the second rig.  Stash
        // throwaway tempdirs to satisfy the Rig shape.
        chat_dir: tempfile::tempdir().expect("placeholder chat_dir"),
        workspace_dir: tempfile::tempdir().expect("placeholder workspace_dir"),
        _handle: handle,
    }
}


// ---------------------------------------------------------------------------
// /api/auth/config — unauthenticated discovery + WWW-Authenticate header
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_config_returns_none_shape_for_dangerous_no_auth() {
    let r = rig().await;
    let resp = get(&format!("{}/api/auth/config", r.base)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["mode"], "none");
    // No WWW-Authenticate even on 401 in this mode.
    let _r = r;
}

#[tokio::test]
async fn auth_config_returns_bearer_shape_unauthenticated() {
    // Bearer mode: GET /api/auth/config must succeed WITHOUT a token
    // because the SPA calls it before it has one.  Body carries just
    // the mode tag — nothing to discover, the operator pasted the
    // plaintext into the browser already.
    let r = rig_with_auth_and_mode(
        hashed_bearer_for_test("s3cret"),
        test_helpers::AuthMode::Bearer,
    )
    .await;
    let resp = get(&format!("{}/api/auth/config", r.base)).await;
    assert_eq!(resp.status(), StatusCode::OK, "no token should still hit auth/config");
    let j = body_json(resp).await;
    assert_eq!(j["mode"], "bearer");
}

#[tokio::test]
async fn auth_config_returns_oidc_shape_with_endpoints_unauthenticated() {
    // OIDC mode: the SPA needs issuer + authorization_endpoint +
    // (optional) token_endpoint + client_id + required_scopes to
    // bootstrap an auth code flow before it has a token.
    let r = rig_with_auth_and_mode(
        Arc::new(DangerousNoAuth), // gate is wide open; the test only cares about the SPA-facing summary
        test_helpers::AuthMode::Oidc {
            issuer: "https://idp.example.com".into(),
            authorization_endpoint: "https://idp.example.com/authorize".into(),
            token_endpoint: Some("https://idp.example.com/token".into()),
            client_id: "dyson-web".into(),
            required_scopes: vec!["openid".into(), "dyson:api".into()],
        },
    )
    .await;
    let resp = get(&format!("{}/api/auth/config", r.base)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["mode"], "oidc");
    assert_eq!(j["issuer"], "https://idp.example.com");
    assert_eq!(j["authorization_endpoint"], "https://idp.example.com/authorize");
    assert_eq!(j["token_endpoint"], "https://idp.example.com/token");
    assert_eq!(j["client_id"], "dyson-web");
    let scopes = j["required_scopes"].as_array().expect("required_scopes is an array");
    assert_eq!(scopes, &vec![serde_json::json!("openid"), serde_json::json!("dyson:api")]);
}

#[tokio::test]
async fn auth_config_omits_token_endpoint_when_idp_did_not_advertise_one() {
    // Pure-implicit-flow IdPs are rare but the controller must not
    // synthesise a `token_endpoint` URL when the discovery doc didn't
    // list one — surfacing a fake one would push the SPA into a
    // broken code-for-token exchange against an unreachable URL.
    let r = rig_with_auth_and_mode(
        Arc::new(DangerousNoAuth),
        test_helpers::AuthMode::Oidc {
            issuer: "https://idp.example.com".into(),
            authorization_endpoint: "https://idp.example.com/authorize".into(),
            token_endpoint: None,
            client_id: "dyson-web".into(),
            required_scopes: vec![],
        },
    )
    .await;
    let resp = get(&format!("{}/api/auth/config", r.base)).await;
    let j = body_json(resp).await;
    assert!(j["token_endpoint"].is_null(), "token_endpoint must be null when missing");
}

#[tokio::test]
async fn unauthorized_carries_bearer_www_authenticate_header() {
    // RFC 6750 challenge: realm + error.  No issuer / as_uri because
    // this is the static-token mode.
    let r = rig_with_auth_and_mode(
        hashed_bearer_for_test("any"),
        test_helpers::AuthMode::Bearer,
    )
    .await;
    let resp = get(&format!("{}/api/conversations", r.base)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let www = resp
        .headers()
        .get("WWW-Authenticate")
        .expect("Bearer mode must set a 401 challenge")
        .to_str()
        .expect("ASCII");
    assert!(www.contains(r#"realm="dyson""#));
    assert!(www.contains(r#"error="invalid_token""#));
    assert!(!www.contains("as_uri"), "static bearer has no IdP to point at");
    assert!(!www.contains("iss="), "static bearer has no issuer");
}

#[tokio::test]
async fn unauthorized_carries_oidc_www_authenticate_header_with_as_uri_and_iss() {
    // OIDC challenge: include as_uri + iss so a non-browser client
    // (curl wrapper, k6 load test, terraform provider) can find its
    // way to the IdP without out-of-band config.  The auth gate has
    // to actually reject for the 401 to fire — use a HashedBearer as
    // the validator so a bare GET without a token comes back 401.
    let r = rig_with_auth_and_mode(
        hashed_bearer_for_test("placeholder"),
        test_helpers::AuthMode::Oidc {
            issuer: "https://idp.example.com".into(),
            authorization_endpoint: "https://idp.example.com/authorize".into(),
            token_endpoint: Some("https://idp.example.com/token".into()),
            client_id: "dyson-web".into(),
            required_scopes: vec![],
        },
    )
    .await;
    let resp = get(&format!("{}/api/conversations", r.base)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let www = resp
        .headers()
        .get("WWW-Authenticate")
        .expect("OIDC mode must set a 401 challenge")
        .to_str()
        .expect("ASCII");
    assert!(www.contains(r#"realm="dyson""#));
    assert!(www.contains(r#"error="invalid_token""#));
    assert!(www.contains(r#"as_uri="https://idp.example.com/authorize""#));
    assert!(www.contains(r#"iss="https://idp.example.com""#));
}

#[tokio::test]
async fn unauthorized_omits_www_authenticate_for_dangerous_no_auth() {
    // Construct a rig where validation will reject every request even
    // though the SPA-facing AuthMode is None.  Without a challenge
    // header the SPA reads the body's `unauthorized` and decides
    // whether to redirect to login on its own.
    struct AlwaysDeny;
    #[async_trait::async_trait]
    impl Auth for AlwaysDeny {
        async fn validate_request(
            &self,
            _h: &hyper::HeaderMap,
        ) -> dyson::error::Result<dyson::auth::AuthInfo> {
            Err(dyson::error::DysonError::Config("unauthorized".into()))
        }
    }
    let r = rig_with_auth_and_mode(Arc::new(AlwaysDeny), test_helpers::AuthMode::None).await;
    let resp = get(&format!("{}/api/conversations", r.base)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        resp.headers().get("WWW-Authenticate").is_none(),
        "AuthMode::None must not set a challenge header",
    );
}

// ---------------------------------------------------------------------------
// SSE access_token query-param folding
// ---------------------------------------------------------------------------

/// Same as `create_chat` but sends an Authorization header so it
/// works against a Bearer-protected rig.
async fn create_chat_with_token(r: &Rig, title: &str, token: &str) -> String {
    let body = serde_json::json!({ "title": title });
    let bytes = serde_json::to_vec(&body).expect("serialize");
    let resp = request_with_headers(
        &format!("{}/api/conversations", r.base),
        Method::POST,
        Some(bytes),
        &[("authorization", &format!("Bearer {token}"))],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "create_chat must succeed with token");
    let j = body_json(resp).await;
    j["id"].as_str().expect("chat id").to_string()
}

#[tokio::test]
async fn sse_raw_bearer_in_query_no_longer_authorises() {
    // Regression: the old shape folded `?access_token=<bearer>` into
    // a synthetic Authorization header, which meant a real bearer /
    // OIDC token would land in browser history, proxy logs and the
    // referrer chain.  After the migration the query param is
    // exclusively a one-shot ticket — pasting the raw bearer there
    // must NOT authenticate.
    let r = rig_with_auth_and_mode(
        hashed_bearer_for_test("right-token"),
        test_helpers::AuthMode::Bearer,
    )
    .await;
    let chat_id = create_chat_with_token(&r, "x", "right-token").await;
    let url = format!(
        "{}/api/conversations/{}/events?access_token=right-token",
        r.base, chat_id,
    );
    let resp = get(&url).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "raw bearer in URL must no longer authorise the SSE open",
    );
}

#[tokio::test]
async fn sse_access_token_query_param_with_wrong_token_still_401s() {
    let r = rig_with_auth_and_mode(
        hashed_bearer_for_test("right"),
        test_helpers::AuthMode::Bearer,
    )
    .await;
    let chat_id = create_chat_with_token(&r, "x", "right").await;
    let url = format!(
        "{}/api/conversations/{}/events?access_token=wrong",
        r.base, chat_id,
    );
    let resp = get(&url).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn access_token_query_param_only_applies_to_events_path() {
    // A bare `?access_token=` on a non-events path must NOT bypass the
    // header check — the SSE-ticket consumer only fires for paths
    // ending in `/events`.  Otherwise a leaked log line with
    // `?access_token=…` in the query would be a credentials disclosure.
    let r = rig_with_auth_and_mode(
        hashed_bearer_for_test("right-token"),
        test_helpers::AuthMode::Bearer,
    )
    .await;
    let url = format!("{}/api/conversations?access_token=right-token", r.base);
    let resp = get(&url).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "query token must NOT authorise non-events paths",
    );
}

#[tokio::test]
async fn sse_ticket_round_trips_and_is_single_use() {
    // POST /api/auth/sse-ticket mints a one-shot, short-lived,
    // identity-bound token.  EventSource can't send headers, so the
    // SPA sends the ticket as `?access_token=<ticket>`.  The
    // controller looks the ticket up, removes it (single-use), and
    // attaches the bound identity to the request.  Re-using the
    // ticket must 401.
    let r = rig_with_auth_and_mode(
        hashed_bearer_for_test("real-token"),
        test_helpers::AuthMode::Bearer,
    )
    .await;
    let chat_id = create_chat_with_token(&r, "ticketed", "real-token").await;
    let mint = request_with_headers(
        &format!("{}/api/auth/sse-ticket", r.base),
        Method::POST,
        None,
        &[("authorization", "Bearer real-token")],
    )
    .await;
    assert_eq!(mint.status(), StatusCode::OK, "ticket mint must succeed");
    let ticket = body_json(mint).await["ticket"]
        .as_str()
        .expect("ticket field")
        .to_string();
    assert!(!ticket.is_empty(), "ticket must be non-empty");

    // First use authorises the SSE open.
    let url = format!(
        "{}/api/conversations/{}/events?access_token={}",
        r.base, chat_id, ticket,
    );
    let resp = get(&url).await;
    assert_eq!(resp.status(), StatusCode::OK, "first use authorises");

    // Second use fails — single-use semantics.
    let resp = get(&url).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "ticket reuse must 401");
}

#[tokio::test]
async fn sse_ticket_endpoint_requires_auth() {
    // Minting a ticket is itself an authenticated operation.  Without
    // a token, the endpoint must 401.
    let r = rig_with_auth_and_mode(
        hashed_bearer_for_test("real-token"),
        test_helpers::AuthMode::Bearer,
    )
    .await;
    let resp = post_json(
        &format!("{}/api/auth/sse-ticket", r.base),
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn sse_authorization_header_still_authorises_without_a_ticket() {
    // Header-bearer clients (k6, curl wrappers) that don't bother
    // with the ticket flow must still authenticate with a normal
    // Authorization header on the SSE open — the ticket path is a
    // browser-only concession to the EventSource API's lack of
    // headers, not a replacement for the canonical bearer flow.
    let r = rig_with_auth_and_mode(
        hashed_bearer_for_test("real-token"),
        test_helpers::AuthMode::Bearer,
    )
    .await;
    let chat_id = create_chat_with_token(&r, "x", "real-token").await;
    let url = format!("{}/api/conversations/{}/events", r.base, chat_id);
    let resp =
        get_with_header(&url, "authorization", "Bearer real-token").await;
    assert_eq!(resp.status(), StatusCode::OK, "header bearer must still authorise");
}

// ---------------------------------------------------------------------------
// Tuple-match dispatch — every route resolves; unknown routes 404, bogus
// methods 405.  Most of the route-resolves checks already live elsewhere
// in this file; this stitches the negative cases together so the dispatch
// matrix has explicit coverage.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dispatch_returns_405_for_known_route_with_wrong_method() {
    let r = rig().await;
    // /api/conversations only takes GET / POST.
    let resp = request(
        &format!("{}/api/conversations", r.base),
        Method::PUT,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    let resp = request(
        &format!("{}/api/conversations", r.base),
        Method::PATCH,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn dispatch_returns_405_for_unknown_api_path() {
    // Anything under `/api/` that doesn't match a route falls through
    // to method_not_allowed (the catch-all in dispatch_inner) — which
    // means non-GET catches it; GET would land in serve_static, which
    // 404s for unknown asset paths but does NOT 405.
    let r = rig().await;
    let resp = request(
        &format!("{}/api/no-such-thing", r.base),
        Method::POST,
        Some(b"{}".to_vec()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn dispatch_returns_404_for_unknown_get_path_via_static_fallback() {
    let r = rig().await;
    let resp = get(&format!("{}/api/no-such-thing", r.base)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// safe_store_id boundary fuzzing — URL-decoded traversal must not escape.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_endpoints_reject_url_decoded_path_traversal() {
    // dispatch hands the URL-decoded id to safe_store_id; verify
    // every traversal shape an attacker is likely to try lands on
    // a 404 (not 200, not 500).
    let r = rig().await;
    let cases = [
        // %2F → '/'
        "%2F..%2Fetc%2Fpasswd",
        // double-encoded
        "%252F..%252Fetc%252Fpasswd",
        // bare `..`
        "..",
        // dotted segments after the slash strips don't appear here
        // because dispatch picks the LAST segment, but %00 must reject.
        "f1%00",
        // backslash isn't in the alphabet
        "f1%5C",
        // Whitespace
        "f1%20",
        // Unicode (won't decode to ASCII alphanumeric)
        "%C3%A9",
    ];
    for raw in cases {
        let resp = get(&format!("{}/api/files/{}", r.base, raw)).await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "/api/files/{raw} must 404, not traverse",
        );
        let resp = get(&format!("{}/api/artefacts/{}", r.base, raw)).await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "/api/artefacts/{raw} must 404, not traverse",
        );
    }
}

#[tokio::test]
async fn shareable_artefact_redirect_rejects_traversal_ids() {
    // /artefacts/<id> redirects to /#/artefacts/<id> for the SPA
    // reader.  An invalid id must NOT round-trip through Location
    // — that would let an attacker craft a phishing URL on this
    // host that forwards to an arbitrary fragment.
    let r = rig().await;
    let resp = get(&format!("{}/artefacts/%2Fevil", r.base)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Hot-reload propagation through /api/providers AND post_model
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hot_reload_propagates_through_post_model_runtime_override() {
    // /api/model swaps the provider+model on every loaded chat AND
    // installs a runtime override so the next agent built from
    // settings inherits the choice.  The override has to survive a
    // hot-reload of `state.settings` too — without that, a config
    // change racing /api/model would silently discard the override.
    use dyson::config::{LlmProvider, ProviderConfig};
    let r = rig().await;
    let _id = create_chat(&r, "hot reload model").await;

    // Add a second provider via hot-reload.
    let mut next = r.state.settings_snapshot();
    next.providers.insert(
        "anthropic".into(),
        ProviderConfig {
            provider_type: LlmProvider::OpenRouter,
            api_key: Credential::new("sk-anth".into()),
            base_url: None,
            models: vec!["claude-haiku-4-5".to_string()],
        },
    );
    r.state.replace_settings_for_test(next);

    // /api/providers must surface the new entry immediately.
    let resp = get(&format!("{}/api/providers", r.base)).await;
    let providers = body_json(resp).await;
    let names: Vec<&str> = providers
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["id"].as_str())
        .collect();
    assert!(names.contains(&"anthropic"), "hot-reload must surface anthropic; got {names:?}");

    // /api/model on the new provider must succeed.  No persistence
    // (config_path is None in the test rig) but the runtime override
    // is set in-process.
    let resp = post_json(
        &format!("{}/api/model", r.base),
        &serde_json::json!({
            "provider": "anthropic",
            "model": "claude-haiku-4-5",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["provider"], "anthropic");
    assert_eq!(body["model"], "claude-haiku-4-5");

    // Re-list providers — the active one must now be anthropic
    // (the runtime override wins over settings.agent.provider).
    let resp = get(&format!("{}/api/providers", r.base)).await;
    let after = body_json(resp).await;
    let active: Option<&str> = after
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["active"] == true)
        .and_then(|p| p["id"].as_str());
    assert_eq!(active, Some("anthropic"), "post_model must flip the active provider");
}

// ---------------------------------------------------------------------------
// list_artefacts disk fallback — long sessions evict from the FIFO cache
// but the listing endpoint must still surface every persisted entry.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_artefacts_returns_evicted_entries_via_disk_fallback() {
    // Emit more artefacts than the in-memory cap (MAX_ARTEFACTS = 32)
    // so the FIFO ring boots an entry from the cache.  The previous
    // shape walked only `store.order`, which silently dropped the
    // evicted entry from the listing endpoint.  After the fix
    // list_artefacts walks the chat's `artefacts/` subdir on disk and
    // merges with whatever's still cached.
    let r = rig().await;
    let id = create_chat(&r, "many artefacts").await;

    const MAX_ARTEFACTS: usize = 32;
    const TOTAL: usize = MAX_ARTEFACTS + 4; // 36 — four past the cap

    for n in 0..TOTAL {
        let artefact = dyson::message::Artefact::markdown(
            dyson::message::ArtefactKind::Other,
            format!("report-{n:02}"),
            format!("body-{n}"),
        );
        test_helpers::emit_agent_artefact(r.state.clone(), &id, artefact)
            .await
            .expect("emit");
    }

    let list = body_json(
        get(&format!("{}/api/conversations/{}/artefacts", r.base, id)).await,
    )
    .await;
    let arr = list.as_array().expect("array");
    assert_eq!(
        arr.len(),
        TOTAL,
        "every persisted artefact must appear in the listing; got {} of {TOTAL}",
        arr.len(),
    );

    // Newest first — the freshly-emitted "report-35" must lead.
    assert_eq!(arr[0]["title"], format!("report-{:02}", TOTAL - 1));
    // The oldest entry — pushed out of the in-memory FIFO long ago —
    // must still be present, demonstrating the disk fallback works.
    let titles: Vec<&str> = arr.iter().filter_map(|p| p["title"].as_str()).collect();
    assert!(titles.contains(&"report-00"), "evicted artefact must surface; titles: {titles:?}");
}

// ---------------------------------------------------------------------------
// CRLF / quote injection in response headers — sanitisation defense.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unauthorized_strips_crlf_from_oidc_issuer_in_www_authenticate() {
    // A misconfigured (or attacker-controlled) issuer URL with CRLF
    // bytes must not let the value break out of `iss="..."` and inject
    // a sibling header.  Sanitisation runs in `unauthorized()`.
    let r = rig_with_auth_and_mode(
        hashed_bearer_for_test("placeholder"),
        test_helpers::AuthMode::Oidc {
            issuer: "https://idp.example.com\r\nX-Evil: yes".into(),
            authorization_endpoint: "https://idp.example.com/authorize".into(),
            token_endpoint: None,
            client_id: "dyson-web".into(),
            required_scopes: vec![],
        },
    )
    .await;

    let resp = get(&format!("{}/api/conversations", r.base)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // No injected sibling — only the canonical WWW-Authenticate.
    assert!(resp.headers().get("X-Evil").is_none(), "no injected header allowed");
    let www = resp
        .headers()
        .get("WWW-Authenticate")
        .expect("WWW-Authenticate set")
        .to_str()
        .expect("ASCII");
    assert!(!www.contains('\r') && !www.contains('\n'), "CRLF must be stripped");
    // The remaining issuer text without the injection must still be
    // present (sanitiser removes only the dangerous bytes, not the
    // operator's actual host).
    assert!(www.contains("idp.example.com"), "issuer host preserved");
}

#[tokio::test]
async fn baseline_security_headers_present_on_every_response() {
    // Defense-in-depth headers attached after dispatch — every route
    // (API, static, SSE) sees nosniff + Referrer-Policy + DENY on
    // framing + a CSP that locks the renderer to its own origin.
    let r = rig().await;

    // Static shell — index.html.
    let resp = get(&format!("{}/", r.base)).await;
    let h = resp.headers().clone();
    assert_eq!(h.get("X-Content-Type-Options").map(|v| v.to_str().unwrap()), Some("nosniff"));
    assert_eq!(h.get("Referrer-Policy").map(|v| v.to_str().unwrap()), Some("no-referrer"));
    assert_eq!(h.get("X-Frame-Options").map(|v| v.to_str().unwrap()), Some("DENY"));
    let csp = h.get("Content-Security-Policy").expect("CSP header").to_str().unwrap();
    assert!(csp.contains("default-src 'self'"), "CSP missing default-src: {csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "CSP missing frame-ancestors: {csp}");

    // JSON API — /api/conversations.
    let resp = get(&format!("{}/api/conversations", r.base)).await;
    let h = resp.headers().clone();
    assert_eq!(h.get("X-Content-Type-Options").map(|v| v.to_str().unwrap()), Some("nosniff"));
    assert_eq!(h.get("X-Frame-Options").map(|v| v.to_str().unwrap()), Some("DENY"));
    assert!(h.get("Content-Security-Policy").is_some());
    assert!(h.get("Referrer-Policy").is_some());

    // SSE endpoint — must keep text/event-stream and still carry CSP.
    let id = create_chat(&r, "sse-headers").await;
    let url = format!("{}/api/conversations/{}/events", r.base, id);
    // Issue request but only inspect headers; close the body promptly.
    let resp = request(&url, Method::GET, None).await;
    let h = resp.headers().clone();
    assert_eq!(
        h.get("Content-Type").map(|v| v.to_str().unwrap()).unwrap_or(""),
        "text/event-stream",
    );
    assert!(h.get("Content-Security-Policy").is_some(), "SSE response must still carry CSP");
    assert_eq!(h.get("X-Content-Type-Options").map(|v| v.to_str().unwrap()), Some("nosniff"));
}

#[tokio::test]
async fn write_endpoints_reject_oversized_bodies_with_400() {
    // Each upload-bearing endpoint has a per-route cap that backs the
    // body reader.  Anything just over the cap must come back 400 with
    // the size message; anything under the cap must succeed.  Caps:
    //   /api/conversations create          16 KiB
    //   /api/conversations/<id>/feedback   16 KiB
    //   /api/model                         16 KiB
    //   /api/mind/file                      4 MiB
    let r = rig().await;
    let id = create_chat(&r, "size").await;

    // 1) POST /api/conversations — 16 KiB cap.  Build a body whose
    // pad alone is larger than the cap so the encoded JSON is
    // unambiguously over.
    let just_over = serde_json::to_vec(&serde_json::json!({
        "title": "ok",
        "pad": "x".repeat(16 * 1024 + 256),
    })).unwrap();
    let resp = request(
        &format!("{}/api/conversations", r.base),
        Method::POST,
        Some(just_over),
    ).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "create over cap → 400");

    // 2) POST /api/conversations/<id>/feedback — 16 KiB cap.
    let just_over_fb = serde_json::to_vec(&serde_json::json!({
        "turn_index": 0,
        "emoji": "👍",
        "pad": "y".repeat(16 * 1024),
    })).unwrap();
    let resp = request(
        &format!("{}/api/conversations/{id}/feedback", r.base),
        Method::POST,
        Some(just_over_fb),
    ).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "feedback over cap → 400");

    // 3) POST /api/model — 16 KiB cap.
    let just_over_model = serde_json::to_vec(&serde_json::json!({
        "provider": "default",
        "model": "qwen/qwen3.6-plus",
        "pad": "z".repeat(16 * 1024),
    })).unwrap();
    let resp = request(
        &format!("{}/api/model", r.base),
        Method::POST,
        Some(just_over_model),
    ).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "model over cap → 400");

    // 4) POST /api/mind/file — 4 MiB cap.  Build a body whose
    // `content` is 4 MiB + 1, which puts the total over the cap.
    let big_content = "a".repeat(4 * 1024 * 1024 + 1);
    let just_over_mind = serde_json::to_vec(&serde_json::json!({
        "path": "_size.md",
        "content": big_content,
    })).unwrap();
    let resp = request(
        &format!("{}/api/mind/file", r.base),
        Method::POST,
        Some(just_over_mind),
    ).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "mind over cap → 400");

    // Sanity: a body just under the conversations cap still succeeds.
    let under_create = serde_json::to_vec(&serde_json::json!({
        "title": "still-ok",
        "pad": "p".repeat(8 * 1024),
    })).unwrap();
    let resp = request(
        &format!("{}/api/conversations", r.base),
        Method::POST,
        Some(under_create),
    ).await;
    assert_eq!(resp.status(), StatusCode::OK, "under-cap create succeeds");
}

#[tokio::test]
async fn loopback_dangerous_no_auth_rejects_foreign_host_header() {
    // DNS-rebinding defence: with a loopback bind in DangerousNoAuth
    // mode, the Host header must name a loopback host.  A browser
    // page on `attacker.example.com` whose JS resolves the hostname
    // to 127.0.0.1 (DNS rebinding) would otherwise fire requests at
    // the loopback API with `Host: attacker.example.com` and have
    // them accepted.  The controller refuses with 421.
    let r = rig().await;
    let resp = request_with_headers(
        &format!("{}/api/conversations", r.base),
        Method::GET,
        None,
        &[("host", "attacker.example.com")],
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::MISDIRECTED_REQUEST,
        "foreign Host on loopback DangerousNoAuth must be 421",
    );
}

#[tokio::test]
async fn loopback_dangerous_no_auth_accepts_loopback_host_header() {
    // Match: the rig binds 127.0.0.1, so a Host pointing to the same
    // address (with the assigned port) is the legitimate request the
    // browser makes when typing http://127.0.0.1:<port>/ in the
    // address bar.  Must succeed.
    let r = rig().await;
    let port = r.base.rsplit(':').next().unwrap();
    let resp = request_with_headers(
        &format!("{}/api/conversations", r.base),
        Method::GET,
        None,
        &[("host", &format!("127.0.0.1:{port}"))],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "loopback Host must be accepted");
}

#[tokio::test]
async fn bearer_mode_accepts_foreign_host_header() {
    // We don't break reverse-proxy deployments: a controller behind
    // an authenticating reverse proxy sees an arbitrary Host (the
    // public hostname).  Only loopback DangerousNoAuth tightens the
    // gate, because that's where the rebinding threat exists.
    let auth = hashed_bearer_for_test("sekrit");
    let r = rig_with_auth_and_mode(auth, test_helpers::AuthMode::Bearer).await;
    // Authenticate with the matching bearer token, but send a
    // mismatched Host to verify the host gate doesn't fire here.
    let resp = request_with_headers(
        &format!("{}/api/conversations", r.base),
        Method::GET,
        None,
        &[
            ("host", "dyson.example.com"),
            ("authorization", "Bearer sekrit"),
        ],
    )
    .await;
    // Either 200 (auth ok) or 401 (auth wrong); the *important* thing
    // is that it isn't 421 — the host gate must stay off in this mode.
    assert_ne!(
        resp.status(),
        StatusCode::MISDIRECTED_REQUEST,
        "bearer mode must not gate on Host",
    );
}

#[tokio::test]
async fn post_mind_file_rejects_traversal_paths() {
    // Path::join doesn't sanitise `..` or absolute paths, so a body
    // with `path: "../etc/passwd"` would let the workspace writer
    // clobber files outside its root.  The handler must reject these
    // before calling Workspace::set / save.
    let r = rig().await;
    let attack_paths = [
        "",
        "../etc/passwd",
        "a/../b",
        "/etc/passwd",
        "\\\\share\\evil",
        "a\0b",
        "a\\b",
    ];
    for p in attack_paths {
        let resp = post_json(
            &format!("{}/api/mind/file", r.base),
            &serde_json::json!({ "path": p, "content": "x" }),
        ).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "path {p:?} must be rejected",
        );
    }
    // Sanity: workspace root is undisturbed — no file with the attack
    // basename leaked into it.
    let leaked = r.workspace_dir.path().join("passwd");
    assert!(!leaked.exists(), "traversal must not write outside workspace");
}

#[tokio::test]
async fn get_mind_file_rejects_traversal_paths() {
    // Same gate on the read side: a `path` query like `../etc/passwd`
    // would otherwise let the operator (or attacker on a non-loopback
    // bind) read arbitrary files via Workspace::get.
    let r = rig().await;
    for p in ["../etc/passwd", "/etc/passwd", "a/../b", "a\\b"] {
        let url = format!(
            "{}/api/mind/file?path={}",
            r.base,
            urlencoding_minimal(p),
        );
        let resp = get(&url).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "path {p:?} must be rejected");
    }
}

/// Tiny percent-encoder for the few bytes that confuse URL parsers in
/// the test rig (`/`, `\\`, `..`, NUL).  Good enough for these tests —
/// pulling in `urlencoding` for one call site would bloat the dev tree.
fn urlencoding_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(*b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[tokio::test]
async fn sse_replay_with_last_event_id_resumes_from_checkpoint() {
    // The chat handle keeps a rolling buffer of the last N events
    // tagged with monotonic ids.  A reconnecting EventSource sends
    // `Last-Event-ID: <n>` and only sees events newer than that id.
    // Without the buffer the client misses everything emitted while
    // it was reconnecting — a 30-second-old `tool_result` would never
    // re-arrive on the wire.
    let r = rig().await;
    let id = create_chat(&r, "replay").await;

    // Drive a few events into the chat handle directly via the
    // test_helpers artefact emit — same shape the agent uses for
    // SseEvent::Artefact.  Three artefacts → three events with
    // monotonic ids 1..3.
    for n in 0..3 {
        let art = dyson::message::Artefact::markdown(
            dyson::message::ArtefactKind::Other,
            format!("a-{n}"),
            format!("body-{n}"),
        );
        test_helpers::emit_agent_artefact(r.state.clone(), &id, art)
            .await
            .expect("emit");
    }

    // Reconnect with Last-Event-ID: 1 — the controller should replay
    // events 2 and 3 (and only those).  We read the response body
    // with a 1s timeout to avoid hanging on the open SSE stream.
    let url = format!("{}/api/conversations/{id}/events", r.base);
    let resp = request_with_headers(
        &url,
        Method::GET,
        None,
        &[("Last-Event-ID", "1")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // Drain a few frames — the replay yields each backlog event as a
    // separate body frame.  Stop once we've seen the buffered set or
    // the timeout hits.
    let mut body = String::new();
    let mut stream = resp.into_body();
    for _ in 0..6 {
        match tokio::time::timeout(
            std::time::Duration::from_millis(300),
            stream.frame(),
        )
        .await
        {
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    body.push_str(&String::from_utf8_lossy(&data));
                }
            }
            _ => break,
        }
        if body.contains("id: 2") && body.contains("id: 3") {
            break;
        }
    }
    assert!(body.contains("id: 2"), "must replay event id 2; body: {body}");
    assert!(body.contains("id: 3"), "must replay event id 3; body: {body}");
    assert!(!body.contains("id: 1\n"), "must not replay events the client already saw; body: {body}");
}

#[tokio::test]
async fn unauthorized_drops_www_authenticate_when_value_is_invalid_for_header() {
    // hyper's HeaderValue::from_str refuses bytes outside RFC 7230's
    // visible-ASCII set.  An OIDC config carrying e.g. embedded NULs
    // (an attacker-controlled / corrupted issuer JSON) used to make
    // the unauthorized() builder panic via .unwrap().  The fixed
    // shape skips the WWW-Authenticate header for that response and
    // still emits a clean 401 — the rest of the auth surface is
    // unaffected.
    let r = rig_with_auth_and_mode(
        hashed_bearer_for_test("placeholder"),
        test_helpers::AuthMode::Oidc {
            // NUL is illegal for HeaderValue but legal in a String —
            // the production code path used to panic on this input.
            issuer: "https://idp.example.com\u{0000}".into(),
            authorization_endpoint: "https://idp.example.com/authorize".into(),
            token_endpoint: None,
            client_id: "dyson-web".into(),
            required_scopes: vec![],
        },
    )
    .await;
    let resp = get(&format!("{}/api/conversations", r.base)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "still 401, no panic");
    assert!(
        resp.headers().get("WWW-Authenticate").is_none(),
        "must not emit a malformed WWW-Authenticate header",
    );
}

#[tokio::test]
async fn unauthorized_strips_quote_from_oidc_authorization_endpoint() {
    // A `"` in the URL would close `as_uri="..."` and let the rest of
    // the value land outside the parameter — defence-in-depth.
    let r = rig_with_auth_and_mode(
        hashed_bearer_for_test("placeholder"),
        test_helpers::AuthMode::Oidc {
            issuer: "https://idp.example.com".into(),
            authorization_endpoint: "https://idp.example.com/authorize\" injected=\"x".into(),
            token_endpoint: None,
            client_id: "dyson-web".into(),
            required_scopes: vec![],
        },
    )
    .await;

    let resp = get(&format!("{}/api/conversations", r.base)).await;
    let www = resp
        .headers()
        .get("WWW-Authenticate")
        .expect("challenge")
        .to_str()
        .expect("ASCII");
    // The breakout signature an attacker would aim for is the exact
    // sequence `" injected="` — closing the as_uri quoted-param and
    // opening a sibling param.  Sanitisation strips the `"` from the
    // value, so the post-fix header may still contain `injected=`
    // (now safely inside the as_uri value) but never the closing-
    // quote-then-new-param sequence.  This is strictly stronger than
    // quote-counting, which can stay even either way.
    assert!(
        !www.contains(r#"" injected=""#),
        "breakout sequence must be defanged; got: {www}",
    );
    // The legitimate `realm="dyson"` parameter must still be present —
    // sanitisation only strips bytes from operator-supplied URLs.
    assert!(www.contains(r#"realm="dyson""#));
}

