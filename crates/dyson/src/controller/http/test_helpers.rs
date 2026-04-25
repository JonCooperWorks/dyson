// ===========================================================================
// HTTP controller — test-only constructors and event-emission helpers.
//
// `pub` items here are part of `dyson::controller::http::test_helpers`
// (gated `#[doc(hidden)]` from the parent), exposing just enough of
// the controller's internals so the integration tests in
// `crates/dyson/tests/http_controller.rs` can drive every code path
// without standing up a real LLM turn or binding a TCP port at the
// public address the operator configured.
//
// Always compiled (cfg(test) is per-crate; integration tests can't
// see it) but `#[doc(hidden)]` on the module keeps it out of the
// public docs.
// ===========================================================================

use std::sync::Arc;

use tokio::net::TcpListener;

use crate::auth::Auth;
use crate::chat_history::ChatHistory;
use crate::config::Settings;
use crate::feedback::FeedbackStore;

use super::ClientRegistry;
use super::Output;
use super::output::SseOutput;
use super::state::HttpState;

pub use super::wire::AuthMode;

pub fn build_state(
    settings: Settings,
    registry: Arc<ClientRegistry>,
    history: Option<Arc<dyn ChatHistory>>,
    feedback: Option<Arc<FeedbackStore>>,
    auth: Arc<dyn Auth>,
) -> Arc<HttpState> {
    build_state_with_auth_mode(settings, registry, history, feedback, auth, AuthMode::None)
}

/// Same as `build_state` but lets the test pin a specific `AuthMode`
/// so the unauthenticated `/api/auth/config` discovery endpoint and the
/// `WWW-Authenticate` header on a 401 carry the values the test wants
/// to assert against.  The `auth: Arc<dyn Auth>` and the `AuthMode`
/// are independent: the trait object is the validation gate, the
/// `AuthMode` is the SPA-facing summary the controller surfaces to
/// the browser before any credential is presented.
pub fn build_state_with_auth_mode(
    settings: Settings,
    registry: Arc<ClientRegistry>,
    history: Option<Arc<dyn ChatHistory>>,
    feedback: Option<Arc<FeedbackStore>>,
    auth: Arc<dyn Auth>,
    auth_mode: AuthMode,
) -> Arc<HttpState> {
    // Mirror the production derivation of the loopback Host-header
    // gate: only on when the operator runs DangerousNoAuth on a
    // loopback bind (every test rig binds 127.0.0.1).  Bearer / OIDC
    // rigs leave the gate off so reverse-proxy-shaped tests don't
    // 421.
    let loopback_only_host_check = matches!(auth_mode, AuthMode::None);
    Arc::new(HttpState::new(
        settings,
        registry,
        history,
        feedback,
        auth,
        auth_mode,
        None,
        loopback_only_host_check,
    ))
}

pub async fn serve(state: Arc<HttpState>, listener: TcpListener) -> crate::Result<()> {
    super::serve_loop(state, listener).await
}

/// Drive the `image_generate` / agent `send_file` path from a test
/// without standing up a real LLM turn: look up the chat, build a
/// one-shot `SseOutput` over its broadcast channel, and call the
/// same `Output::send_file` the agent would.  Round-trips through
/// `FileStore` so `/api/files/<id>` serves the bytes afterwards.
pub async fn emit_agent_file(
    state: Arc<HttpState>,
    chat_id: &str,
    path: &std::path::Path,
) -> crate::Result<()> {
    emit_agent_file_for_tool(state, chat_id, path, None).await
}

/// Variant of `emit_agent_file` that simulates emission during a
/// specific tool call — stamps the artefact entry with the given
/// `tool_use_id` exactly like the live agent loop would after
/// `Output::tool_use_start`.  Used by the image-generate
/// tool-panel round-trip test.
pub async fn emit_agent_file_for_tool(
    state: Arc<HttpState>,
    chat_id: &str,
    path: &std::path::Path,
    tool_use_id: Option<&str>,
) -> crate::Result<()> {
    let handle = state
        .chats
        .lock()
        .await
        .get(chat_id)
        .cloned()
        .ok_or_else(|| crate::DysonError::Config(format!("no chat {chat_id}")))?;
    let mut out = SseOutput {
        chat_id: chat_id.to_string(),
        tx: handle.events.clone(),
        replay: Arc::clone(&handle.replay),
        files: state.files.clone(),
        next_file_id: state.file_id.clone(),
        artefacts: state.artefacts.clone(),
        next_artefact_id: state.artefact_id.clone(),
        data_dir: state.data_dir.clone(),
        current_tool_use_id: tool_use_id.map(|s| s.to_string()),
    };
    out.send_file(path)
}

/// Drive the cross-controller `BrowserArtefactSink` path from a
/// test — lets the integration tests verify that a file sent
/// through Telegram's `send_file` would land in the web UI's
/// Artefacts tab.  Returns the minted `(file_id, artefact_id)` so
/// the caller can assert on `/api/files/...` and
/// `/api/artefacts/...` reachability.
pub fn publish_file_as_artefact_for_test(
    state: Arc<HttpState>,
    chat_id: &str,
    path: &std::path::Path,
) -> Option<(String, String)> {
    state.publish_file_as_artefact_impl(chat_id, path)
}

/// Mirror of `emit_agent_file` for artefacts: stash the given
/// artefact in the controller's store and emit an SSE event over
/// the chat's broadcast channel.  Used by integration tests to
/// validate the full round-trip without standing up a real
/// subagent.
pub async fn emit_agent_artefact(
    state: Arc<HttpState>,
    chat_id: &str,
    artefact: crate::message::Artefact,
) -> crate::Result<()> {
    let handle = state
        .chats
        .lock()
        .await
        .get(chat_id)
        .cloned()
        .ok_or_else(|| crate::DysonError::Config(format!("no chat {chat_id}")))?;
    let mut out = SseOutput {
        chat_id: chat_id.to_string(),
        tx: handle.events.clone(),
        replay: Arc::clone(&handle.replay),
        files: state.files.clone(),
        next_file_id: state.file_id.clone(),
        artefacts: state.artefacts.clone(),
        next_artefact_id: state.artefact_id.clone(),
        data_dir: state.data_dir.clone(),
        current_tool_use_id: None,
    };
    out.send_artefact(&artefact)
}

/// Write a fixture transcript straight to the configured chat
/// history backend.  Used by tests that need realistic messages
/// without standing up an LLM — `role` is either `"user"` or
/// `"assistant"`.  Panics on unknown role (test-only helper).
pub async fn seed_transcript(
    state: Arc<HttpState>,
    chat_id: &str,
    messages: &[(&str, &str)],
) -> crate::Result<()> {
    use crate::message::{ContentBlock, Message, Role};
    let history = state
        .history
        .as_ref()
        .cloned()
        .ok_or_else(|| crate::DysonError::Config("no chat_history backend".into()))?;
    let msgs: Vec<Message> = messages
        .iter()
        .map(|(role, text)| {
            let role = match *role {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                other => panic!("unknown role in seed_transcript: {other}"),
            };
            Message {
                role,
                content: vec![ContentBlock::Text {
                    text: (*text).to_string(),
                }],
            }
        })
        .collect();
    history.save(chat_id, &msgs)
}

/// Reach the per-chat activity handle a real turn would receive.
/// Used by integration tests to drive the Activity registry
/// without standing up a subagent tool call.
pub fn activity_handle(
    state: &HttpState,
    chat_id: &str,
) -> crate::controller::ActivityHandle {
    state.activity.handle_for(chat_id)
}

/// Accessor for the raw registry — lets tests assert directly
/// against its snapshot methods (verifies restart-survival and
/// stale-Running reconciliation without going through HTTP).
pub fn activity_registry(state: &HttpState) -> Arc<crate::controller::ActivityRegistry> {
    Arc::clone(&state.activity)
}

/// Spin until the chat's broadcast channel has at least one
/// subscriber — the only way to close the race between a client
/// connecting to `/events` and a producer emitting into the
/// channel (broadcast drops events that have no receivers).
pub async fn wait_for_sse_subscriber(state: Arc<HttpState>, chat_id: &str) {
    for _ in 0..200 {
        if let Some(h) = state.chats.lock().await.get(chat_id).cloned() {
            if h.events.receiver_count() > 0 {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no SSE subscriber for chat {chat_id} after 2s");
}
