// ===========================================================================
// HTTP controller — web UI + JSON API + SSE event stream.
//
// Hosts the React UI at `/` (always served from the embedded build —
// `build.rs` picks up frontend changes via mtime-gated `npm run build`)
// and exposes a small JSON API plus per-conversation Server-Sent Events:
//
//   GET  /                          → index.html (Vite-built bundle)
//   GET  /assets/*                  → hashed JS/CSS/font chunks
//   GET  /api/conversations         → list this controller's chats
//   POST /api/conversations         → create new chat → { id, title }
//   GET  /api/conversations/:id     → load Vec<MessageDto>
//   POST /api/conversations/:id/turn{prompt} → 202; events stream via SSE
//   POST /api/conversations/:id/cancel       → cooperative cancellation
//   GET  /api/conversations/:id/events       → SSE stream of agent events
//   GET  /api/providers             → providers from settings
//   GET  /api/skills                → tool inventory (from a representative agent)
//
// Conversations live in memory for the controller's lifetime.  Persistence
// to ChatHistory is a future addition; for now the focus is talking to a
// real agent in a browser.  Bind to 127.0.0.1 by default.
//
// Inbound auth: on a loopback bind the `auth` field is optional and
// defaults to `DangerousNoAuth` (the loopback threat model is a single
// trusted operator).  On any other bind the field is required;
// omitting it refuses to start.  `DangerousNoAuth` is the opt-in
// escape hatch, modeled on `--dangerous-no-sandbox`; `Bearer` is
// enforced on every `/api/*` request via the shared `Auth` trait.
// ===========================================================================

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::auth::{Auth, DangerousNoAuth, HashedBearerAuth, OidcAuth};
use crate::chat_history::{ChatHistory, create_chat_history};
use crate::config::{ControllerConfig, Settings};
use crate::error::DysonError;
use crate::feedback::{FeedbackEntry, FeedbackRating, FeedbackStore};
use crate::message::{ContentBlock, Message, Role};
use crate::util::resolve_tilde;

use super::{AgentMode, ClientRegistry, Controller, Output, build_agent};

mod assets;
mod config;
mod output;
mod responses;
mod state;
mod stores;
mod wire;

use config::{HttpAuthConfig, HttpControllerConfigRaw, is_loopback_bind};
use output::SseOutput;
use responses::{
    Resp, auth_headers_for, bad_request, boxed, client_accepts_gzip, get_auth_config, json_ok,
    maybe_gzip, method_not_allowed, mime_for_extension, not_found, parse_query, read_json,
    read_json_capped, safe_store_id, sanitize_filename, unauthorized, url_decode,
};
pub use state::HttpState;
use state::ChatHandle;
use stores::{ArtefactStore, FileStore};
use wire::{
    ArtefactDto, AuthMode, BlockDto, ConversationDto, CreateChatBody, FeedbackBody, MAX_TURN_BODY,
    MessageDto, MindWriteBody, ModelSwitchBody, ProviderDto, SseEvent, TurnBody,
};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------


// ---------------------------------------------------------------------------
// HttpController
// ---------------------------------------------------------------------------

pub struct HttpController {
    bind: String,
    init: AuthInit,
}

/// What `from_config` parsed out of the operator's `auth` block.
/// `Ready` covers everything we can build synchronously: it holds both
/// the live `Auth` impl and the public-discovery shape the SPA needs.
/// `PendingOidc` parks an OIDC config until `run()` can `.await` the
/// `.well-known` fetch — discovery there fails the controller fast
/// rather than silently returning 401s.
enum AuthInit {
    Ready {
        auth: Arc<dyn Auth>,
        mode: AuthMode,
    },
    PendingOidc {
        issuer: String,
        audience: String,
        required_scopes: Vec<String>,
    },
}

impl HttpController {
    pub fn from_config(config: &ControllerConfig) -> Option<Self> {
        if config.controller_type != "http" {
            return None;
        }
        let raw: HttpControllerConfigRaw = match serde_json::from_value(config.config.clone()) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "failed to parse http controller config — is `auth` set? \
                     (use {{\"type\":\"dangerous_no_auth\"}} to run unauthenticated)"
                );
                return None;
            }
        };
        let auth_config = match raw.auth {
            Some(a) => a,
            None => {
                // Loopback gets the break: a single trusted operator is
                // the loopback threat model, so unset `auth` defaults to
                // DangerousNoAuth.  Any other bind must name a mechanism
                // — otherwise we'd be silently exposing an
                // unauthenticated endpoint.
                if is_loopback_bind(&raw.bind) {
                    HttpAuthConfig::DangerousNoAuth
                } else {
                    tracing::error!(
                        bind = %raw.bind,
                        "http controller: non-loopback bind requires an explicit `auth` field \
                         (use {{\"type\":\"dangerous_no_auth\"}} to run unauthenticated, or \
                         {{\"type\":\"bearer\",\"token\":\"...\"}} to require a token)"
                    );
                    return None;
                }
            }
        };
        let init = match auth_config {
            HttpAuthConfig::DangerousNoAuth => AuthInit::Ready {
                auth: Arc::new(DangerousNoAuth),
                mode: AuthMode::None,
            },
            HttpAuthConfig::Bearer { hash } => {
                if hash.is_empty() {
                    tracing::error!("http controller: bearer auth configured with empty hash");
                    return None;
                }
                match HashedBearerAuth::from_phc(hash) {
                    Ok(a) => AuthInit::Ready {
                        auth: Arc::new(a),
                        mode: AuthMode::Bearer,
                    },
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "http controller: bearer hash is not a valid argon2 PHC string \
                             (generate one with `dyson hash-bearer <plaintext>`)"
                        );
                        return None;
                    }
                }
            }
            HttpAuthConfig::Oidc {
                issuer,
                audience,
                required_scopes,
            } => {
                if issuer.is_empty() || audience.is_empty() {
                    tracing::error!(
                        "http controller: oidc auth requires non-empty issuer and audience"
                    );
                    return None;
                }
                AuthInit::PendingOidc {
                    issuer,
                    audience,
                    required_scopes,
                }
            }
        };
        Some(Self { bind: raw.bind, init })
    }
}

#[async_trait::async_trait]
impl Controller for HttpController {
    fn name(&self) -> &str {
        "http"
    }

    async fn run(
        &self,
        settings: &Settings,
        registry: &Arc<ClientRegistry>,
    ) -> crate::Result<()> {
        // Build the live auth + its public-discovery shape.  For OIDC
        // we run `.well-known/openid-configuration` here, in async
        // land, so a misconfigured issuer fails the controller start
        // rather than leaving the endpoint silently rejecting every
        // request — same posture as the bearer hash check at
        // config-parse time.
        let (auth, auth_mode): (Arc<dyn Auth>, AuthMode) = match &self.init {
            AuthInit::Ready { auth, mode } => (Arc::clone(auth), mode.clone()),
            AuthInit::PendingOidc {
                issuer,
                audience,
                required_scopes,
            } => {
                let built = OidcAuth::discover(
                    issuer,
                    audience.clone(),
                    required_scopes.clone(),
                    None,
                )
                .await?;
                let mode = AuthMode::Oidc {
                    issuer: built.issuer().to_string(),
                    authorization_endpoint: built.authorization_endpoint().to_string(),
                    token_endpoint: built.token_endpoint().map(str::to_string),
                    client_id: audience.clone(),
                    required_scopes: required_scopes.clone(),
                };
                tracing::info!(
                    issuer = %built.issuer(),
                    "http controller: oidc auth discovered"
                );
                (Arc::new(built) as Arc<dyn Auth>, mode)
            }
        };

        // Build the persistent ChatHistory; tolerate failure (controller
        // still works in memory-only mode).
        let history: Option<Arc<dyn ChatHistory>> =
            match create_chat_history(&settings.chat_history) {
                Ok(h) => Some(Arc::from(h)),
                Err(e) => {
                    tracing::warn!(error = %e, "http controller: chat history disabled");
                    None
                }
            };

        // Feedback store sits in the same directory as ChatHistory so
        // ratings made here land in the same files Telegram writes to.
        let feedback: Option<Arc<FeedbackStore>> = if history.is_some() {
            let dir = resolve_tilde(settings.chat_history.connection_string.expose());
            Some(Arc::new(FeedbackStore::new(dir)))
        } else {
            None
        };

        // Resolve the dyson.json path the operator started with so
        // `post_model` can persist the web UI's choice the same way
        // Telegram's /model command does.  Matches the resolution
        // used by `create_hot_reloader` (which HTTP doesn't itself
        // use since it has no per-process agent cache to flush, but
        // the path is still the right one to write back to).
        let config_path = std::env::args()
            .skip_while(|a| a != "--config" && a != "-c")
            .nth(1)
            .map(PathBuf::from)
            .or_else(|| {
                let p = PathBuf::from("dyson.json");
                if p.exists() { Some(p) } else { None }
            });

        let state = Arc::new(HttpState::new(
            settings.clone(),
            Arc::clone(registry),
            history.clone(),
            feedback.clone(),
            Arc::clone(&auth),
            auth_mode,
            config_path,
        ));

        // Expose the artefact store across controllers so a file sent
        // through Telegram's `send_file` lands in the web UI's
        // Artefacts tab for the same chat id.  First installer wins —
        // running multiple HTTP controllers in one process is
        // unsupported.
        super::install_browser_artefact_sink(
            Arc::clone(&state) as Arc<dyn super::BrowserArtefactSink>,
        );

        // Hydrate the chat list from disk so existing conversations show
        // up immediately in the left rail.
        if let Some(h) = history.as_ref() {
            match h.list() {
                Ok(ids) => {
                    let mut chats = state.chats.lock().await;
                    let mut order = state.order.lock().await;
                    for id in ids {
                        // Title heuristic: use the first user-text turn,
                        // or fall back to the chat id.  Cheap because we
                        // only load at startup.
                        let title = match h.load(&id) {
                            Ok(msgs) => first_user_text(&msgs)
                                .unwrap_or_else(|| id.clone()),
                            Err(_) => id.clone(),
                        };
                        let handle = Arc::new(ChatHandle::new(title));
                        chats.insert(id.clone(), handle);
                        order.push(id);
                    }
                    tracing::info!(
                        count = order.len(),
                        "http controller: hydrated chats from disk"
                    );
                }
                Err(e) => tracing::warn!(error = %e, "chat history list failed"),
            }
        }

        let listener = TcpListener::bind(&self.bind).await.map_err(|e| {
            DysonError::Config(format!("failed to bind {} for http controller: {e}", self.bind))
        })?;

        tracing::info!(
            bind = %self.bind,
            "HTTP controller listening — open http://{} in a browser",
            self.bind,
        );

        // Probe the configured auth to log which mechanism is active.
        // DangerousNoAuth is the only variant that validates an empty
        // HeaderMap successfully; treat that success as the loud warning
        // signal.  Everything else falls through to the info branch.
        let empty_headers = hyper::HeaderMap::new();
        match auth.validate_request(&empty_headers).await {
            Ok(info) if info.identity == "anonymous" => {
                tracing::warn!(
                    bind = %self.bind,
                    "HTTP controller running with DangerousNoAuth — every \
                     request is accepted. Only safe on loopback."
                );
            }
            _ => {
                tracing::info!(bind = %self.bind, "HTTP controller inbound auth enforced");
            }
        }

        // Subscribe to the program-level hot-reload broadcast (owned
        // by `command::listen`).  A single watcher polls dyson.json,
        // reloads the shared ClientRegistry, and publishes a fresh
        // `Arc<Settings>` here so every controller sees the change
        // without each running its own file watcher.  If nothing is
        // broadcasting (tests, controllers-that-don't-plumb-it-in),
        // the task becomes a no-op.
        if let Some(mut rx) = super::subscribe_settings_updates() {
            let state_for_task = Arc::clone(&state);
            tokio::spawn(async move {
                while rx.changed().await.is_ok() {
                    let fresh = rx.borrow().clone();
                    if let Ok(mut guard) = state_for_task.settings.write() {
                        *guard = (*fresh).clone();
                    }
                    tracing::info!("http controller: settings hot-reloaded");
                }
            });
        }

        serve_loop(state, listener).await
    }
}

/// Accept loop, factored out of `Controller::run` so integration tests
/// can drive their own pre-bound listener (and discover the OS-assigned
/// port via `local_addr`).  Never returns under normal operation;
/// returns `Ok(())` only if the accept loop is cancelled by dropping
/// the task.
async fn serve_loop(state: Arc<HttpState>, listener: TcpListener) -> crate::Result<()> {
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "http accept error");
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req| {
                let state = Arc::clone(&state);
                async move { Ok::<_, Infallible>(dispatch(req, state).await) }
            });
            if let Err(e) = http1::Builder::new().serve_connection(io, svc).await {
                tracing::debug!(error = %e, "http connection ended");
            }
        });
    }
}

// Test-only constructor that lets integration tests build state with
// custom paths (temp dirs) and drive it with `serve_loop`.  Always
// compiled (cfg(test) is per-crate, integration tests can't see it),
// but `#[doc(hidden)]` keeps it out of the public docs surface.
#[doc(hidden)]
pub mod test_helpers {
    use super::*;

    pub fn build_state(
        settings: Settings,
        registry: Arc<ClientRegistry>,
        history: Option<Arc<dyn ChatHistory>>,
        feedback: Option<Arc<FeedbackStore>>,
        auth: Arc<dyn Auth>,
    ) -> Arc<HttpState> {
        Arc::new(HttpState::new(
            settings,
            registry,
            history,
            feedback,
            auth,
            AuthMode::None,
            None,
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
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

async fn dispatch(req: Request<hyper::body::Incoming>, state: Arc<HttpState>) -> Resp {
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
        (&Method::GET,    ["api", "conversations"])                 => list_conversations(&state).await,
        (&Method::POST,   ["api", "conversations"])                 => create_conversation(req, &state).await,
        (&Method::GET,    ["api", "conversations", id])             => get_conversation(&state, id).await,
        (&Method::DELETE, ["api", "conversations", id])             => delete_conversation(&state, id).await,
        (&Method::POST,   ["api", "conversations", id, "turn"])     => post_turn(req, Arc::clone(&state), id).await,
        (&Method::POST,   ["api", "conversations", id, "cancel"])   => post_cancel(&state, id).await,
        (&Method::GET,    ["api", "conversations", id, "events"])   => sse_events(&state, id).await,
        (&Method::GET,    ["api", "conversations", id, "feedback"]) => get_feedback(&state, id).await,
        (&Method::POST,   ["api", "conversations", id, "feedback"]) => post_feedback(req, &state, id).await,
        (&Method::GET,    ["api", "conversations", id, "artefacts"]) => list_artefacts(&state, id).await,
        (&Method::GET,    ["api", "conversations", id, "export"])   => export_conversation(&state, id).await,

        // ─── providers / model / mind / activity ───────────────────────
        (&Method::GET,    ["api", "providers"])    => list_providers(&state),
        (&Method::POST,   ["api", "model"])        => post_model(req, Arc::clone(&state)).await,
        (&Method::GET,    ["api", "mind"])         => get_mind(&state).await,
        (&Method::GET,    ["api", "mind", "file"]) => get_mind_file(&state, req.uri().query().unwrap_or("")).await,
        (&Method::POST,   ["api", "mind", "file"]) => post_mind_file(req, &state).await,
        (&Method::GET,    ["api", "activity"])     => get_activity(&state, req.uri().query().unwrap_or("")),

        // ─── files & artefacts ─────────────────────────────────────────
        (&Method::GET, ["api", "files", id])     => get_file(&state, &url_decode(id)).await,
        (&Method::GET, ["api", "artefacts", id]) => get_artefact(&state, &url_decode(id)).await,
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
        (&Method::GET, _) => serve_static(&state, &path).await,
        _ if path.starts_with("/api/") => method_not_allowed(),
        _ => method_not_allowed(),
    }
}

// ---------------------------------------------------------------------------
// API: conversations
// ---------------------------------------------------------------------------

async fn list_conversations(state: &HttpState) -> Resp {
    // Prefer the disk's mtime-sorted list when a ChatHistory is
    // configured — Telegram and HTTP share the same on-disk chat dir,
    // so asking disk rather than our in-memory `order` vec means a
    // message sent on Telegram bubbles that chat to the top of the
    // HTTP sidebar at the next list call.  `disk::list()` already
    // sorts newest-first by `transcript.json` mtime.
    let disk_order: Option<Vec<String>> = state
        .history
        .as_ref()
        .and_then(|h| h.list().ok());
    let mut order = match disk_order {
        Some(o) if !o.is_empty() => o,
        _ => state.order.lock().await.clone(),
    };
    // Merge in any in-memory chat ids the disk didn't surface (brand
    // new, transcript not yet flushed) so a just-minted HTTP chat
    // still shows up immediately.
    {
        let mem_order = state.order.lock().await;
        let seen: std::collections::HashSet<&str> =
            order.iter().map(String::as_str).collect();
        let extras: Vec<String> = mem_order
            .iter()
            .filter(|id| !seen.contains(id.as_str()))
            .cloned()
            .collect();
        for id in extras.into_iter().rev() {
            order.insert(0, id);
        }
    }

    // Hydrate handles for chat ids we learned about from disk
    // (typically Telegram chats created while this process was
    // running).  Title is a best-effort read of the first user-text
    // line; a missing/corrupt transcript falls back to the id.
    {
        let mut chats = state.chats.lock().await;
        for id in order.iter() {
            if chats.contains_key(id) {
                continue;
            }
            let title = state
                .history
                .as_ref()
                .and_then(|h| h.load(id).ok())
                .and_then(|msgs| first_user_text(&msgs))
                .unwrap_or_else(|| id.clone());
            chats.insert(id.clone(), Arc::new(ChatHandle::new(title)));
        }
    }

    // Build a set of chat ids that own at least one artefact.  Cheap
    // because it's just the in-memory index plus a one-shot scan of
    // each chat's `artefacts/` subdir for chats whose reports have
    // aged out of the FIFO cache.
    let mut with_artefacts: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    if let Ok(store) = state.artefacts.lock() {
        for entry in store.items.values() {
            with_artefacts.insert(entry.chat_id.clone());
        }
    }
    if let Some(dir) = state.data_dir.as_ref() {
        for id in order.iter() {
            if with_artefacts.contains(id) {
                continue;
            }
            let sub = ArtefactStore::dir_for_chat(dir, id);
            if std::fs::read_dir(&sub)
                .into_iter()
                .flatten()
                .flatten()
                .any(|e| {
                    e.path()
                        .extension()
                        .is_some_and(|x| x == "json")
                })
            {
                with_artefacts.insert(id.clone());
            }
        }
    }
    let chats = state.chats.lock().await;
    let mut dtos = Vec::with_capacity(order.len());
    for id in order.iter() {
        if let Some(h) = chats.get(id) {
            dtos.push(ConversationDto {
                id: id.clone(),
                title: h.title.clone(),
                live: h.busy.load(std::sync::atomic::Ordering::Relaxed),
                has_artefacts: with_artefacts.contains(id),
                source: source_for_chat_id(id),
            });
        }
    }
    json_ok(&dtos)
}

/// Classify a chat id by its mint convention.  HTTP-minted ids are
/// `c-NNNN` (see `mint_id`); everything else is a Telegram chat id
/// (bare numeric string from `teloxide::types::ChatId`).  Used by the
/// conversation DTO so the sidebar can badge Telegram rows.
fn source_for_chat_id(id: &str) -> &'static str {
    if id.starts_with("c-") {
        "http"
    } else {
        "telegram"
    }
}

async fn create_conversation(req: Request<hyper::body::Incoming>, state: &HttpState) -> Resp {
    let body: CreateChatBody = match read_json(req).await {
        Ok(b) => b,
        Err(e) => return bad_request(&e),
    };
    // Rotate the caller-supplied previous chat first so "+ New
    // Conversation" produces a dated archive the same way /clear does.
    // Best-effort: a missing chat or IO error is logged but doesn't
    // block creation.  The in-memory agent (if any) gets its messages
    // cleared so a future turn on that id doesn't resurrect stale
    // context from the agent cache.
    if let Some(prev) = body.rotate_previous.as_deref() {
        if let Some(prev_handle) = state.chats.lock().await.get(prev).cloned() {
            if let Some(agent) = prev_handle.agent.lock().await.as_mut() {
                agent.clear();
            }
        }
        if let Some(h) = state.history.as_ref() {
            if let Err(e) = h.rotate(prev) {
                tracing::warn!(error = %e, chat_id = %prev, "failed to rotate previous chat");
            }
            // Keep the rotated chat visible across restarts by seeding
            // an empty current file — otherwise `list()` skips it and
            // the sidebar loses both the chat and its artefacts.
            if let Err(e) = h.save(prev, &[]) {
                tracing::warn!(error = %e, chat_id = %prev, "failed to seed empty chat after rotate");
            }
        }
    }
    let id = state.mint_id().await;
    let title = body.title.unwrap_or_else(|| "New conversation".to_string());
    let handle = Arc::new(ChatHandle::new(title.clone()));
    state.chats.lock().await.insert(id.clone(), handle);
    // Newest first — push to front so the sidebar shows new chats on top.
    state.order.lock().await.insert(0, id.clone());
    // Persist immediately so every conversation lives on disk 1:1 with
    // the in-memory list.  Without this an empty chat vanishes on
    // restart — the user would see "1 chat" in the sidebar, restart,
    // and the chat would be gone because nothing was ever saved.  The
    // save is best-effort: an IO failure is logged but doesn't fail
    // creation (the in-memory chat still works for this session).
    if let Some(h) = state.history.as_ref() {
        if let Err(e) = h.save(&id, &[]) {
            tracing::warn!(error = %e, chat_id = %id, "failed to persist new chat");
        }
    }
    json_ok(&serde_json::json!({ "id": id, "title": title }))
}

/// Move `id` to the front of the order list.  Called after every turn
/// so the most recently active chat sits on top.  No-op if the id
/// isn't in the list (shouldn't happen, but cheap to guard).
async fn bump_to_front(state: &HttpState, id: &str) {
    let mut order = state.order.lock().await;
    if let Some(pos) = order.iter().position(|x| x == id) {
        if pos != 0 {
            let entry = order.remove(pos);
            order.insert(0, entry);
        }
    }
}

async fn get_conversation(state: &HttpState, id: &str) -> Resp {
    let handle = match state.chats.lock().await.get(id).cloned() {
        Some(h) => h,
        None => return not_found(),
    };
    let agent_guard = handle.agent.lock().await;
    let mut messages: Vec<MessageDto> = match agent_guard.as_ref() {
        // Agent already loaded for this chat — its messages are the truth.
        Some(a) => a.messages().iter().map(message_to_dto).collect(),
        // Agent not built yet — load straight from disk so the transcript
        // shows even before the user types in this session.
        None => match state.history.as_ref() {
            Some(h) => match h.load(id) {
                Ok(msgs) => msgs.iter().map(message_to_dto).collect(),
                Err(_) => Vec::new(),
            },
            None => Vec::new(),
        },
    };
    drop(agent_guard);

    // Artefacts are side-channel — they never land in the conversation
    // history, so a fresh page load from disk shows no chips.  Walk the
    // ArtefactStore for this chat and append a synthetic assistant
    // turn with one `Artefact` block per entry so the chat scroll
    // preserves image / report chips across browser refreshes and
    // controller restarts.
    let artefact_blocks: Vec<BlockDto> = {
        let store = match state.artefacts.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        store
            .order
            .iter()
            .filter_map(|aid| store.items.get(aid).map(|e| (aid, e)))
            .filter(|(_, e)| e.chat_id == id)
            .map(|(aid, e)| BlockDto::Artefact {
                id: aid.clone(),
                kind: e.kind,
                title: e.title.clone(),
                url: format!("/#/artefacts/{aid}"),
                bytes: e.content.len(),
                tool_use_id: e.tool_use_id.clone(),
                metadata: e.metadata.clone(),
            })
            .collect()
    };
    if !artefact_blocks.is_empty() {
        messages.push(MessageDto {
            role: "assistant".to_string(),
            blocks: artefact_blocks,
        });
    }

    json_ok(&serde_json::json!({
        "id": id,
        "title": handle.title,
        "messages": messages,
    }))
}

/// Pluck the first user-text block from a message list — used as a chat
/// title hint when hydrating from disk.  Truncated to 60 chars.
fn first_user_text(messages: &[Message]) -> Option<String> {
    for m in messages {
        if matches!(m.role, Role::User) {
            for b in &m.content {
                if let ContentBlock::Text { text } = b {
                    let mut t: String = text.lines().next().unwrap_or("").to_string();
                    if t.chars().count() > 60 {
                        t = t.chars().take(60).collect::<String>() + "…";
                    }
                    if !t.is_empty() {
                        return Some(t);
                    }
                }
            }
        }
    }
    None
}

fn message_to_dto(m: &Message) -> MessageDto {
    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
    .to_string();
    let blocks = m.content.iter().map(block_to_dto).collect();
    MessageDto { role, blocks }
}

fn block_to_dto(b: &ContentBlock) -> BlockDto {
    match b {
        ContentBlock::Text { text } => BlockDto::Text { text: text.clone() },
        ContentBlock::Thinking { thinking } => BlockDto::Thinking {
            thinking: thinking.clone(),
        },
        ContentBlock::ToolUse { id, name, input } => BlockDto::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => BlockDto::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.clone(),
            is_error: *is_error,
        },
        ContentBlock::Artefact { id, kind, title } => BlockDto::Artefact {
            id: id.clone(),
            kind: *kind,
            title: title.clone(),
            url: format!("/#/artefacts/{id}"),
            bytes: 0,
            tool_use_id: None,
            metadata: None,
        },
        // User-uploaded image from chat history.  Emit as a data URL
        // so the FileBlock renders inline without a second round-trip.
        // Chat history already shrinks the transcript itself by
        // externalising these to `{chat_dir}/media/<hash>.b64` — we're
        // just the last-hop re-hydration for the browser.
        ContentBlock::Image { data, media_type } => {
            // Rough decoded byte count: base64 is ~4/3 of the raw size.
            let bytes = data.len().saturating_mul(3) / 4;
            BlockDto::File {
                name: format!("image.{}", image_ext_for(media_type)),
                mime: media_type.clone(),
                bytes,
                url: format!("data:{media_type};base64,{data}"),
                inline_image: true,
            }
        }
        // PDFs: render as a download chip.  The extracted text lives
        // in `extracted_text` but isn't useful to surface inline in
        // the transcript — the download link lets the user open the
        // original.
        ContentBlock::Document {
            data,
            extracted_text,
        } => {
            let bytes = data.len().saturating_mul(3) / 4;
            BlockDto::File {
                name: if extracted_text.is_empty() {
                    "document.pdf".to_string()
                } else {
                    // Cheap title: first non-empty line of the extract,
                    // truncated.  Falls back to `document.pdf`.
                    let title = extracted_text
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("document.pdf")
                        .trim()
                        .chars()
                        .take(60)
                        .collect::<String>();
                    if title.is_empty() {
                        "document.pdf".to_string()
                    } else {
                        format!("{title}.pdf")
                    }
                },
                mime: "application/pdf".to_string(),
                bytes,
                url: format!("data:application/pdf;base64,{data}"),
                inline_image: false,
            }
        }
    }
}

/// Best-effort MIME-to-extension mapping for user-uploaded images.
/// Falls back to `png` for unknown types so the browser at least has
/// something to save under when the user clicks the attachment.
fn image_ext_for(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/heic" => "heic",
        _ => "png",
    }
}

// ---------------------------------------------------------------------------
// API: turn (kick off agent.run; events stream over SSE)
// ---------------------------------------------------------------------------

async fn post_turn(
    req: Request<hyper::body::Incoming>,
    state: Arc<HttpState>,
    id: &str,
) -> Resp {
    // Reject oversized bodies before buffering — a 100MB upload would
    // pin a request worker and waste memory.
    if let Some(cl) = req.headers().get("content-length")
        && let Some(len) = cl.to_str().ok().and_then(|s| s.parse::<usize>().ok())
        && len > MAX_TURN_BODY
    {
        return bad_request(&format!("request body too large ({len} bytes; max {MAX_TURN_BODY})"));
    }
    let body: TurnBody = match read_json_capped(req, MAX_TURN_BODY).await {
        Ok(b) => b,
        Err(e) => return bad_request(&e),
    };

    // Decode attachments up front so a malformed base64 fails the
    // request before we kick off the agent (clean rejection > orphan
    // SSE done event).
    let mut decoded: Vec<crate::media::Attachment> = Vec::with_capacity(body.attachments.len());
    for a in &body.attachments {
        match base64::engine::general_purpose::STANDARD.decode(a.data_base64.as_bytes()) {
            Ok(bytes) => decoded.push(crate::media::Attachment {
                data: bytes,
                mime_type: a.mime_type.clone(),
                file_name: a.name.clone(),
            }),
            Err(e) => return bad_request(&format!("attachment '{}' base64 decode failed: {e}",
                a.name.as_deref().unwrap_or("<unnamed>"))),
        }
    }

    let handle = match state.chats.lock().await.get(id).cloned() {
        Some(h) => h,
        None => return not_found(),
    };

    // Intercept `/clear` before the busy latch + spawn path.  Without
    // this, the slash command listed in data.js would land at the LLM
    // as a plain prompt and nothing on disk would rotate.  Telegram's
    // `handle_per_chat_command` does the same thing via
    // `execute_agent_command` → `chat_store.rotate`.  Other slash
    // commands (`/compact`, `/model`) require an LLM call or have
    // dedicated endpoints, so they continue to fall through.
    if body.prompt.trim() == "/clear" && decoded.is_empty() {
        if let Some(agent) = handle.agent.lock().await.as_mut() {
            agent.clear();
        }
        if let Some(h) = state.history.as_ref() {
            if let Err(e) = h.rotate(id) {
                tracing::warn!(error = %e, chat_id = %id, "failed to rotate chat history");
            }
            // Re-create the current file as an empty transcript so the
            // chat stays visible across restarts.  Without this,
            // DiskChatHistory::list() skips it (no current file, only
            // archives) and the sidebar loses the chat — along with the
            // artefacts filtered by its id.
            if let Err(e) = h.save(id, &[]) {
                tracing::warn!(error = %e, chat_id = %id, "failed to seed empty chat after rotate");
            }
        }
        let _ = handle.events.send(SseEvent::Done);
        return json_ok(&serde_json::json!({ "ok": true, "cleared": true }));
    }

    if handle
        .busy
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return Response::builder()
            .status(StatusCode::CONFLICT)
            .header("Content-Type", "application/json")
            .body(boxed(Bytes::from_static(
                br#"{"error":"chat is busy"}"#,
            )))
            .unwrap();
    }

    // Set up cancellation.
    let cancel = CancellationToken::new();
    *handle.cancel.lock().await = Some(cancel.clone());

    // Apply the runtime model override before handing settings to
    // `build_agent` so a brand-new chat picks up the operator's last
    // model choice instead of the startup default.  `runtime_model`
    // is set by `post_model`; when unset this is a no-op clone.
    let mut settings = state.settings_snapshot();
    let override_pm: Option<(String, String)> = match state.runtime_model.lock() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    };
    let override_provider_name = if let Some((prov, model)) = override_pm {
        if let Some(pc) = settings.providers.get(&prov) {
            settings.agent.provider = pc.provider_type.clone();
            settings.agent.model = model;
        }
        Some(prov)
    } else {
        None
    };
    let registry = Arc::clone(&state.registry);
    let history = state.history.clone();
    let prompt = body.prompt;
    let attachments = decoded;
    let chat_handle = Arc::clone(&handle);
    let chat_id = id.to_string();
    let state_for_task = Arc::clone(&state);
    let files = Arc::clone(&state.files);
    let file_id = Arc::clone(&state.file_id);
    let artefacts = Arc::clone(&state.artefacts);
    let artefact_id = Arc::clone(&state.artefact_id);
    let data_dir = state.data_dir.clone();

    tokio::spawn(async move {
        let mut output = SseOutput {
            chat_id: chat_id.clone(),
            tx: chat_handle.events.clone(),
            files,
            next_file_id: file_id,
            artefacts,
            next_artefact_id: artefact_id,
            data_dir,
            current_tool_use_id: None,
        };

        // Lazily build the agent on first use.  If a transcript exists
        // on disk for this chat_id, replay it into the agent so context
        // carries across sessions.
        let mut guard = chat_handle.agent.lock().await;
        if guard.is_none() {
            // Prefer the runtime-selected provider's client when one
            // is set — falls back to the registry default otherwise
            // (unknown provider name or no override set).
            let client = match override_provider_name.as_deref() {
                Some(p) => registry.get(p).unwrap_or_else(|_| registry.get_default()),
                None => registry.get_default(),
            };
            match build_agent(&settings, None, AgentMode::Private, client, &registry, None).await {
                Ok(mut a) => {
                    if let Some(h) = history.as_ref() {
                        match h.load(&chat_id) {
                            Ok(msgs) if !msgs.is_empty() => a.set_messages(msgs),
                            _ => {}
                        }
                    }
                    *guard = Some(a);
                }
                Err(e) => {
                    let _ = chat_handle.events.send(SseEvent::LlmError {
                        message: format!("agent build failed: {e}"),
                    });
                    let _ = chat_handle.events.send(SseEvent::Done);
                    chat_handle
                        .busy
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
            }
        }
        let agent = guard.as_mut().expect("agent built above");
        // The agent polls `cancellation.is_cancelled()` at iteration
        // boundaries, which is fine for /stop between tool calls but
        // useless during a long LLM stream or a multi-second tool.
        // Keep a separate clone of the token so the outer `select!`
        // below can tear the whole run down when the user clicks
        // cancel — the agent future drops at its next await point,
        // which cancels in-flight HTTP requests, tool processes, and
        // streaming reads cooperatively.
        let cancel_for_select = cancel.clone();
        agent.set_cancellation_token(cancel);
        // Wire the chat-scoped activity handle so the Activity tab
        // shows running subagents for this chat.  Rebound on every
        // turn (cheap Arc clone) because the agent is cached and
        // re-used, but handle binding carries chat_id which we only
        // know at this dispatch site.
        agent.set_activity_handle(state.activity.handle_for(&chat_id));
        // Checkpoint-save the transcript to disk after every message
        // push.  Without this, a process kill during a long subagent
        // run (e.g. security_engineer streams for minutes) loses the
        // whole conversation — the end-of-turn save below is
        // unreachable if the tokio task is aborted mid-run.
        if let Some(h) = history.as_ref() {
            let h = Arc::clone(h);
            let chat_id_for_hook = chat_id.clone();
            agent.set_persist_hook(std::sync::Arc::new(move |messages| {
                if let Err(e) = h.save(&chat_id_for_hook, messages) {
                    tracing::warn!(error = %e, chat_id = %chat_id_for_hook, "persist hook failed to save chat history");
                }
            }));
        }

        // Branch on attachments: with attachments, dispatch through
        // run_with_attachments so images/audio/PDF are resolved into
        // multimodal ContentBlocks (same path Telegram takes).
        //
        // Wrap in `tokio::select!` so POST /cancel aborts the run at
        // the next await point instead of waiting for the current LLM
        // stream / tool call to finish on its own.  The persist hook
        // installed above has already checkpointed every message the
        // agent committed to its conversation, so dropping the future
        // mid-run is safe: the state that survives is exactly what
        // the agent had decided on.
        let result = tokio::select! {
            biased;
            _ = cancel_for_select.cancelled() => {
                tracing::info!(chat_id = %chat_id, "turn aborted by cancel request");
                let _ = chat_handle.events.send(SseEvent::LlmError {
                    message: "cancelled".to_string(),
                });
                Ok(String::new())
            }
            r = async {
                if attachments.is_empty() {
                    agent.run(&prompt, &mut output).await
                } else {
                    agent.run_with_attachments(&prompt, attachments, &mut output).await
                }
            } => r,
        };
        match result {
            Ok(_) => {}
            Err(e) => {
                let _ = chat_handle.events.send(SseEvent::LlmError {
                    message: e.to_string(),
                });
            }
        }

        // Persist the conversation to disk after every turn.  This is the
        // canonical save point — controllers/telegram does the same.
        if let Some(h) = history.as_ref() {
            if let Err(e) = h.save(&chat_id, agent.messages()) {
                tracing::warn!(error = %e, chat_id = %chat_id, "failed to save chat history");
            }
        }

        let _ = chat_handle.events.send(SseEvent::Done);
        chat_handle
            .busy
            .store(false, std::sync::atomic::Ordering::SeqCst);

        // Bump this chat to the top of the sidebar list — most-recent
        // activity wins.
        bump_to_front(&state_for_task, &chat_id).await;
    });

    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header("Content-Type", "application/json")
        .body(boxed(Bytes::from_static(br#"{"ok":true}"#)))
        .unwrap()
}

async fn delete_conversation(state: &HttpState, id: &str) -> Resp {
    // Sidebar dismiss.  Empty chats (no in-memory agent messages AND
    // no saved transcript) hard-delete their `{id}.json` — otherwise
    // a freshly-minted chat the user cancels leaves a zero-byte file
    // stranded on disk.  Non-empty chats rotate instead so the
    // transcript survives as a dated archive the user can still grep.
    let handle = match state.chats.lock().await.remove(id) {
        Some(h) => h,
        None => return not_found(),
    };
    state.order.lock().await.retain(|x| x != id);

    // Cancel any in-flight turn before we drop the handle so the
    // agent doesn't keep streaming into a chat the sidebar forgot.
    if let Some(cancel) = handle.cancel.lock().await.as_ref() {
        cancel.cancel();
    }

    let in_memory_empty = match handle.agent.lock().await.as_ref() {
        Some(a) => a.messages().is_empty(),
        None => true,
    };

    let mut preserved = false;
    if let Some(h) = state.history.as_ref() {
        let disk_empty = h.load(id).map(|m| m.is_empty()).unwrap_or(true);
        if in_memory_empty && disk_empty {
            if let Err(e) = h.remove(id) {
                tracing::warn!(error = %e, chat_id = %id, "failed to remove empty chat");
            }
        } else {
            if let Err(e) = h.rotate(id) {
                tracing::warn!(error = %e, chat_id = %id, "failed to rotate deleted chat");
            }
            preserved = true;
        }
    }

    json_ok(&serde_json::json!({ "ok": true, "deleted": true, "preserved": preserved }))
}

async fn post_cancel(state: &HttpState, id: &str) -> Resp {
    let handle = match state.chats.lock().await.get(id).cloned() {
        Some(h) => h,
        None => return not_found(),
    };
    if let Some(cancel) = handle.cancel.lock().await.as_ref() {
        cancel.cancel();
    }
    json_ok(&serde_json::json!({ "ok": true }))
}

// ---------------------------------------------------------------------------
// SSE
// ---------------------------------------------------------------------------

async fn sse_events(state: &HttpState, id: &str) -> Resp {
    let handle = match state.chats.lock().await.get(id).cloned() {
        Some(h) => h,
        None => return not_found(),
    };
    let mut rx = handle.events.subscribe();
    // Build the SSE byte stream by hand so we don't depend on
    // tokio-stream's `sync` feature (would add to deps).  Each
    // broadcast::recv outcome → one frame.
    let body_stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(evt) => {
                    yield Ok::<_, Infallible>(Frame::data(Bytes::from(format_sse(&evt))));
                    if matches!(evt, SseEvent::Done) { break; }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    yield Ok(Frame::data(Bytes::from_static(b": lag\n\n")));
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    let body = StreamBody::new(body_stream).boxed();
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(body)
        .unwrap()
}

fn format_sse(evt: &SseEvent) -> String {
    let json = serde_json::to_string(evt).unwrap_or_else(|_| "{}".to_string());
    format!("data: {json}\n\n")
}

// ---------------------------------------------------------------------------
// API: providers, skills
// ---------------------------------------------------------------------------

fn list_providers(state: &HttpState) -> Resp {
    // The startup settings name an active provider + model, but the
    // operator may have switched since then via `POST /api/model` —
    // let the runtime override win so the UI's active-model label
    // matches what actually runs on the next turn.  Snapshot once so
    // the list and the active-model calculation read the same
    // settings (no cross-call torn reads if a hot-reload races this).
    let snapshot = state.settings_snapshot();
    let runtime = state
        .runtime_model
        .lock()
        .ok()
        .and_then(|g| g.clone());
    let active_name = runtime
        .as_ref()
        .map(|(p, _)| p.clone())
        .or_else(|| super::active_provider_name(&snapshot));
    let active_model_override = runtime.as_ref().map(|(_, m)| m.clone());

    let mut dtos: Vec<ProviderDto> = snapshot
        .providers
        .iter()
        .map(|(id, pc)| {
            let is_active = active_name.as_deref() == Some(id.as_str());
            let active_model = if is_active {
                active_model_override
                    .clone()
                    .unwrap_or_else(|| snapshot.agent.model.clone())
            } else {
                pc.models.first().cloned().unwrap_or_default()
            };
            ProviderDto {
                id: id.clone(),
                name: id.clone(),
                models: pc.models.clone(),
                active_model,
                active: is_active,
            }
        })
        .collect();
    dtos.sort_by_key(|p| !p.active);
    json_ok(&dtos)
}

// ---------------------------------------------------------------------------
// Feedback — same emoji set as the Telegram controller.
// ---------------------------------------------------------------------------

fn emoji_to_rating(emoji: &str) -> Option<FeedbackRating> {
    // Mirror crate::controller::telegram::feedback so behaviour matches
    // Telegram exactly.  Kept inline (not re-exported from the telegram
    // module) so the http controller doesn't depend on telegram's wiring.
    match emoji {
        "💩" | "😡" | "🤮"          => Some(FeedbackRating::Terrible),
        "👎"                           => Some(FeedbackRating::Bad),
        "😢" | "😐"                    => Some(FeedbackRating::NotGood),
        "👍" | "👏"                    => Some(FeedbackRating::Good),
        "🔥" | "🎉" | "😂"             => Some(FeedbackRating::VeryGood),
        "❤️" | "❤" | "🤯" | "💯" | "⚡" => Some(FeedbackRating::Excellent),
        _ => None,
    }
}

async fn get_feedback(state: &HttpState, id: &str) -> Resp {
    let entries = match state.feedback.as_ref() {
        Some(fb) => fb.load(id).unwrap_or_default(),
        None => Vec::new(),
    };
    json_ok(&entries)
}

async fn post_feedback(
    req: Request<hyper::body::Incoming>,
    state: &HttpState,
    id: &str,
) -> Resp {
    let body: FeedbackBody = match read_json(req).await {
        Ok(b) => b,
        Err(e) => return bad_request(&e),
    };
    let fb = match state.feedback.as_ref() {
        Some(f) => f,
        None => return bad_request("feedback store not configured"),
    };
    match body.emoji.as_deref().filter(|s| !s.is_empty()) {
        Some(emoji) => {
            let rating = match emoji_to_rating(emoji) {
                Some(r) => r,
                None => return bad_request(&format!("unknown emoji: {emoji}")),
            };
            let entry = FeedbackEntry {
                turn_index: body.turn_index,
                rating,
                score: rating.score(),
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            };
            if let Err(e) = fb.upsert(id, entry) {
                return bad_request(&format!("save failed: {e}"));
            }
            json_ok(&serde_json::json!({ "ok": true, "rating": rating, "emoji": emoji }))
        }
        None => {
            // Empty emoji = remove existing feedback for this turn.
            if let Err(e) = fb.remove(id, body.turn_index) {
                return bad_request(&format!("remove failed: {e}"));
            }
            json_ok(&serde_json::json!({ "ok": true, "removed": true }))
        }
    }
}

// ---------------------------------------------------------------------------
// Static files
// ---------------------------------------------------------------------------

async fn serve_static(_state: &HttpState, path: &str) -> Resp {
    // Decode before the traversal check — raw `%2e%2e%2f` would otherwise
    // slip past `contains("..")` and resolve to `..` when the OS opens
    // the file.  Reject backslashes too; we don't serve on Windows but
    // the embedded-asset lookup is case-sensitive and `\` is never a
    // legitimate URL path byte.
    let decoded = url_decode(path);
    if decoded.contains("..") || decoded.contains('\\') || decoded.contains('\0') {
        return not_found();
    }
    // The frontend is always served from the embedded bundle generated
    // by `build.rs`.  Frontend changes ride into the binary on the next
    // `cargo build` (mtime-gated, so a clean tree is a no-op); there is
    // no on-disk webroot override.
    match assets::lookup(&decoded) {
        Some((bytes, ct)) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", ct)
            .header("Cache-Control", "no-cache")
            .body(boxed(Bytes::from_static(bytes)))
            .unwrap(),
        None => not_found(),
    }
}


// ---------------------------------------------------------------------------
// API: mind / activity
// ---------------------------------------------------------------------------

/// List workspace files for the Mind view.  Pulls metadata from disk
/// (size, modified-at) when the workspace lives on a real filesystem.
async fn get_mind(state: &HttpState) -> Resp {
    let snapshot = state.settings_snapshot();
    let ws = match crate::workspace::create_workspace(&snapshot.workspace) {
        Ok(w) => w,
        Err(e) => return bad_request(&format!("workspace open failed: {e}")),
    };
    let names = ws.list_files();
    let mut files: Vec<serde_json::Value> = Vec::with_capacity(names.len());
    for name in &names {
        files.push(serde_json::json!({
            "path": name,
            "size": ws.get(name).map(|c| c.len()).unwrap_or(0),
        }));
    }
    json_ok(&serde_json::json!({
        "backend": snapshot.workspace.backend,
        "files": files,
    }))
}

/// Return one workspace file's content for the Mind editor pane.
async fn get_mind_file(state: &HttpState, query: &str) -> Resp {
    let path = match parse_query(query).into_iter().find(|(k, _)| k == "path") {
        Some((_, v)) => v,
        None => return bad_request("missing 'path' query parameter"),
    };
    let snapshot = state.settings_snapshot();
    let ws = match crate::workspace::create_workspace(&snapshot.workspace) {
        Ok(w) => w,
        Err(e) => return bad_request(&format!("workspace open failed: {e}")),
    };
    match ws.get(&path) {
        Some(content) => json_ok(&serde_json::json!({ "path": path, "content": content })),
        None => not_found(),
    }
}

/// Serve a previously-stored agent-produced file (image, PoC, etc.).
/// Inline content-disposition for images so they preview in `<img>`;
/// attachment for everything else so the browser downloads.
async fn get_file(state: &HttpState, id: &str) -> Resp {
    // Reject anything outside the minted alphabet (dispatch hands us
    // the URL-decoded value; an attacker submitting `%2F../etc/passwd`
    // would otherwise traverse).  Mint-only ids are `f<u64>`.
    if !safe_store_id(id) {
        return not_found();
    }
    // Check the in-memory cache first, then fall back to disk.  Files
    // evicted from the FIFO cache stay reachable as long as the
    // controller has a data_dir configured — which is always true when
    // the operator has chat_history on.
    let cached = {
        let store = match state.files.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        store
            .items
            .get(id)
            .map(|e| (e.bytes.clone(), e.mime.clone(), e.name.clone()))
    };
    let (bytes, mime, name) = match cached {
        Some(t) => t,
        None => {
            let loaded = state
                .data_dir
                .as_ref()
                .and_then(|dir| FileStore::load_from_disk(dir, id));
            match loaded {
                Some(e) => {
                    // Warm the cache so subsequent hits don't re-read
                    // disk for the same id (browser preview, repeated
                    // downloads, etc.).
                    let out = (e.bytes.clone(), e.mime.clone(), e.name.clone());
                    if let Ok(mut s) = state.files.lock() {
                        s.put(id.to_string(), e);
                    }
                    out
                }
                None => return not_found(),
            }
        }
    };
    let cd = if mime.starts_with("image/") {
        format!("inline; filename=\"{}\"", name.replace('"', ""))
    } else {
        format!("attachment; filename=\"{}\"", name.replace('"', ""))
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", mime)
        .header("Content-Disposition", cd)
        .header("Cache-Control", "no-cache")
        .body(boxed(Bytes::from(bytes)))
        .unwrap()
}

/// Serve the raw markdown body of a stored artefact.  The client-side
/// renderer in `turns.jsx` turns it into HTML; we just hand over the
/// bytes with the right mime type so "copy" / "download" on the reader
/// get what they expect.  Returns 404 when the FIFO has evicted the
/// entry (expected after ~32 reports on a long session) — the UI shows
/// a "no longer in memory — rerun to regenerate" fallback.
async fn get_artefact(state: &HttpState, id: &str) -> Resp {
    if !safe_store_id(id) {
        return not_found();
    }
    let cached = {
        let store = match state.artefacts.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        store.items.get(id).map(|e| {
            (
                e.content.clone().into_bytes(),
                e.mime_type.clone(),
                e.title.clone(),
                e.chat_id.clone(),
            )
        })
    };
    let (bytes, mime, title, chat_id) = match cached {
        Some(t) => t,
        None => {
            let loaded = state
                .data_dir
                .as_ref()
                .and_then(|dir| ArtefactStore::load_from_disk(dir, id));
            match loaded {
                Some(e) => {
                    let out = (
                        e.content.clone().into_bytes(),
                        e.mime_type.clone(),
                        e.title.clone(),
                        e.chat_id.clone(),
                    );
                    if let Ok(mut s) = state.artefacts.lock() {
                        s.put(id.to_string(), e);
                    }
                    out
                }
                None => return not_found(),
            }
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", format!("{mime}; charset=utf-8"))
        .header(
            "Content-Disposition",
            format!("inline; filename=\"{}.md\"", sanitize_filename(&title)),
        )
        // Surfaces the owning chat to the SPA so a direct deep-link
        // (`/#/artefacts/<id>` opened cold) can restore the sidebar
        // context without a second round-trip.
        .header("X-Dyson-Chat-Id", chat_id)
        .header("Cache-Control", "no-cache")
        .body(boxed(Bytes::from(bytes)))
        .unwrap()
}

/// List artefacts for a given chat.  Empty list if none exist yet.
/// Stream a ShareGPT-format dump of a conversation for the web UI's
/// download button.  Reads the transcript from `ChatHistory` (or the
/// in-memory agent's messages if history is absent), folds in the
/// per-turn feedback ratings, and serialises via the same
/// `sharegpt::to_sharegpt_with_feedback` path the `export_conversation`
/// tool uses.  Returns `{"error":..}` JSON on 404 so the bridge can
/// surface the message inline.
async fn export_conversation(state: &HttpState, chat_id: &str) -> Resp {
    // Transcript: prefer disk (authoritative, has everything ever sent
    // for this chat) and fall back to the live agent's in-memory
    // message buffer when no history backend is configured.
    let messages = if let Some(h) = state.history.as_ref() {
        match h.load(chat_id) {
            Ok(m) => m,
            Err(e) => return bad_request(&format!("load transcript: {e}")),
        }
    } else {
        let chats = state.chats.lock().await;
        let Some(handle) = chats.get(chat_id) else {
            return not_found();
        };
        let guard = handle.agent.lock().await;
        match guard.as_ref() {
            Some(a) => a.messages().to_vec(),
            None => Vec::new(),
        }
    };
    if messages.is_empty() {
        return not_found();
    }

    // System prompt mirrors the behaviour of the in-tree tool — use
    // the live agent's current prompt when available so exports
    // capture the persona/role the chat was actually run with.
    let system_prompt: Option<String> = {
        let chats = state.chats.lock().await;
        let handle = chats.get(chat_id).cloned();
        drop(chats);
        if let Some(h) = handle {
            let guard = h.agent.lock().await;
            guard.as_ref().map(|a| a.system_prompt().to_string())
        } else {
            None
        }
    };

    let feedback = state
        .feedback
        .as_ref()
        .and_then(|f| f.load(chat_id).ok())
        .unwrap_or_default();

    let convo = crate::export::sharegpt::to_sharegpt_with_feedback(
        &messages,
        system_prompt.as_deref(),
        Some(chat_id.to_string()),
        &feedback,
    );
    let body = match crate::export::sharegpt::to_sharegpt_json(&[convo]) {
        Ok(s) => s,
        Err(e) => return bad_request(&format!("serialise sharegpt: {e}")),
    };

    let filename = format!("{chat_id}.sharegpt.json");
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json; charset=utf-8")
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", sanitize_filename(&filename)),
        )
        .header("Cache-Control", "no-cache")
        .body(boxed(Bytes::from(body)))
        .unwrap()
}

async fn list_artefacts(state: &HttpState, chat_id: &str) -> Resp {
    let items: Vec<ArtefactDto> = {
        let store = match state.artefacts.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        // Walk `order` back-to-front so the newest sit on top.  The
        // FIFO ordering IS creation order — artefacts never reorder,
        // they only evict from the front.
        store
            .order
            .iter()
            .rev()
            .filter_map(|id| store.items.get(id).map(|e| (id, e)))
            .filter(|(_, e)| e.chat_id == chat_id)
            .map(|(id, e)| ArtefactDto {
                id: id.clone(),
                kind: e.kind,
                title: e.title.clone(),
                bytes: e.content.len(),
                created_at: e.created_at,
                metadata: e.metadata.clone(),
            })
            .collect()
    };
    json_ok(&items)
}

/// Per-lane activity surfaced in the web UI's Activity tab.
///
/// Reads from `HttpState.activity` (the `ActivityRegistry`, disk-backed
/// per chat).  Returns a JSON payload the frontend's `ActivityView`
/// consumes directly:
///
/// ```json
/// { "lanes": [
///     { "lane": "subagent", "name": "security_engineer",
///       "note": "Review crate for OWASP...", "status": "running",
///       "last": "1714053234", "chat_id": "c-0023" },
///     ...
/// ] }
/// ```
///
/// Query params:
/// - `?chat=<id>` — filter to one chat (single-chat view)
/// - default     — all chats, newest-first
///
/// Other lanes (`loop` / `dream` / `swarm`) don't feed this registry
/// yet; the frontend already renders them from separate data sources.
/// Keeping the response schema uniform means extending the registry
/// later is additive, not a rewrite.
fn get_activity(state: &HttpState, query: &str) -> Resp {
    let chat_filter = parse_query(query)
        .into_iter()
        .find(|(k, _)| k == "chat")
        .map(|(_, v)| v);

    let entries = match chat_filter.as_deref() {
        Some(cid) => state.activity.snapshot_chat(cid),
        None => state.activity.snapshot_all(),
    };

    let lanes: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|e| {
            let status = match e.status {
                crate::controller::ActivityStatus::Running => "running",
                crate::controller::ActivityStatus::Ok => "ok",
                crate::controller::ActivityStatus::Err => "err",
            };
            let last = e
                .finished_at
                .map(|t| t.to_string())
                .unwrap_or_else(|| e.started_at.to_string());
            serde_json::json!({
                "lane": e.lane,
                "name": e.name,
                "note": e.note,
                "status": status,
                "last": last,
                "chat_id": e.chat_id,
                "started_at": e.started_at,
                "finished_at": e.finished_at,
            })
        })
        .collect();

    json_ok(&serde_json::json!({ "lanes": lanes }))
}

/// Persist a workspace file edit.  Loads the workspace, calls
/// `Workspace::set` (in-memory) then `save()` (flush).  The agent will
/// see the new content the next time it reads the file — this is the
/// same path the `workspace` tool takes when the agent edits its own
/// mind, so the user and agent share one canonical store.
async fn post_mind_file(req: Request<hyper::body::Incoming>, state: &HttpState) -> Resp {
    let body: MindWriteBody = match read_json(req).await {
        Ok(b) => b,
        Err(e) => return bad_request(&e),
    };
    let snapshot = state.settings_snapshot();
    let mut ws = match crate::workspace::create_workspace(&snapshot.workspace) {
        Ok(w) => w,
        Err(e) => return bad_request(&format!("workspace open failed: {e}")),
    };
    ws.set(&body.path, &body.content);
    if let Err(e) = ws.save() {
        return bad_request(&format!("workspace save failed: {e}"));
    }
    json_ok(&serde_json::json!({ "ok": true, "path": body.path }))
}

/// Switch the LLM provider/model for one chat (or every loaded chat).
/// Calls `Agent::swap_client` on each affected agent — same path the
/// `/model` Telegram command takes.  Future chats still build with the
/// dyson.json default until the controller layer learns to persist
/// per-session overrides.
async fn post_model(req: Request<hyper::body::Incoming>, state: Arc<HttpState>) -> Resp {
    let body: ModelSwitchBody = match read_json(req).await {
        Ok(b) => b,
        Err(e) => return bad_request(&e),
    };
    let snapshot = state.settings_snapshot();
    let provider_cfg = match snapshot.providers.get(&body.provider) {
        Some(c) => c,
        None => return bad_request(&format!("unknown provider '{}'", body.provider)),
    };
    let model = body
        .model
        .clone()
        .or_else(|| provider_cfg.models.first().cloned())
        .unwrap_or_default();
    if model.is_empty() {
        return bad_request("provider has no configured models");
    }
    let provider_type = provider_cfg.provider_type.clone();

    let chats = state.chats.lock().await;
    let targets: Vec<Arc<ChatHandle>> = match body.chat_id {
        Some(id) => match chats.get(&id) {
            Some(h) => vec![Arc::clone(h)],
            None => return not_found(),
        },
        None => chats.values().cloned().collect(),
    };
    drop(chats);

    let mut swapped = 0usize;
    for handle in targets {
        let mut guard = handle.agent.lock().await;
        if let Some(agent) = guard.as_mut() {
            let client = state
                .registry
                .get(&body.provider)
                .unwrap_or_else(|_| state.registry.get_default());
            agent.swap_client(client, &model, &provider_type);
            swapped += 1;
        }
    }

    // Persist to dyson.json so the choice survives a restart and a
    // new conversation picks it up as the default.  Before this
    // fix the web UI silently lost its model switch the next time
    // any code rebuilt an agent from `Settings` — Telegram's
    // `/model` command already writes through the same helper, so
    // without this HTTP the two controllers fought each other.
    if let Some(cp) = state.config_path.as_ref() {
        crate::config::loader::persist_model_selection(cp, &body.provider, &model);
    }
    // In-memory override so the *next* agent this process builds
    // (new chat, first-use hydration, etc.) also picks up the
    // choice without needing a restart.  `state.settings` is
    // frozen from startup; without this, post_model would only
    // affect already-running agents.
    if let Ok(mut slot) = state.runtime_model.lock() {
        *slot = Some((body.provider.clone(), model.clone()));
    }

    json_ok(&serde_json::json!({
        "ok": true,
        "provider": body.provider,
        "model": model,
        "swapped": swapped,
    }))
}

// ---------------------------------------------------------------------------
// Unit tests — pure helpers.  End-to-end HTTP round-trips live in
// `crates/dyson/tests/http_controller.rs` so they can bind a port.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_to_rating_matches_telegram() {
        // Every emoji the Telegram controller honours must round-trip
        // here too — chats started in Telegram are read by the web UI.
        let cases: &[(&str, FeedbackRating)] = &[
            ("💩", FeedbackRating::Terrible),
            ("😡", FeedbackRating::Terrible),
            ("🤮", FeedbackRating::Terrible),
            ("👎", FeedbackRating::Bad),
            ("😢", FeedbackRating::NotGood),
            ("😐", FeedbackRating::NotGood),
            ("👍", FeedbackRating::Good),
            ("👏", FeedbackRating::Good),
            ("🔥", FeedbackRating::VeryGood),
            ("🎉", FeedbackRating::VeryGood),
            ("😂", FeedbackRating::VeryGood),
            ("❤️", FeedbackRating::Excellent),
            ("❤", FeedbackRating::Excellent),
            ("🤯", FeedbackRating::Excellent),
            ("💯", FeedbackRating::Excellent),
            ("⚡", FeedbackRating::Excellent),
        ];
        for (e, want) in cases {
            assert_eq!(emoji_to_rating(e), Some(*want), "emoji {e} should map");
        }
        assert_eq!(emoji_to_rating("🦀"), None);
        assert_eq!(emoji_to_rating(""), None);
    }

    #[test]
    fn first_user_text_picks_first_user_message() {
        let msgs = vec![
            Message::user("hello world"),
            Message::assistant(vec![ContentBlock::Text {
                text: "hi back".into(),
            }]),
        ];
        assert_eq!(first_user_text(&msgs).as_deref(), Some("hello world"));
    }

    #[test]
    fn first_user_text_truncates_long_titles() {
        let long = "a".repeat(200);
        let msgs = vec![Message::user(&long)];
        let title = first_user_text(&msgs).unwrap();
        assert!(title.chars().count() <= 61, "title was {title}");
        assert!(title.ends_with('…'));
    }

    #[test]
    fn first_user_text_skips_assistant_only() {
        let msgs = vec![Message::assistant(vec![ContentBlock::Text {
            text: "no user here".into(),
        }])];
        assert_eq!(first_user_text(&msgs), None);
    }

    // The embedded bundle is part of the binary's contract.  Filenames
    // are hashed by Vite, so these tests verify shape — index.html at
    // the root, at least one JS chunk, at least one CSS chunk — rather
    // than specific paths.  Frontend-facing regressions live in
    // crates/dyson/src/controller/http/web/src/__tests__/ and run
    // under `npm test` (invoked by build.rs as part of `npm run build`).

    #[test]
    fn embedded_bundle_serves_root() {
        let (bytes, ct) = assets::lookup("/").expect("/ must serve index.html");
        assert!(ct.starts_with("text/html"));
        assert!(!bytes.is_empty());
        let body = std::str::from_utf8(bytes).unwrap();
        assert!(body.contains("id=\"root\""), "index.html must mount React at #root");
    }

    #[test]
    fn embedded_bundle_has_js_chunk_and_inlined_css() {
        // CSS is inlined into index.html by the dyson-inline-css Vite
        // plugin — that's what lets us skip the render-blocking CSS
        // round-trip on cold paint.  So the contract here is (a) a JS
        // chunk exists and (b) the HTML carries the stylesheet rules
        // directly, NOT as a separate .css asset.
        let has_js = assets::ASSETS.iter().any(|(p, _, ct)| {
            p.ends_with(".js") && ct.starts_with("application/javascript")
        });
        let has_standalone_css = assets::ASSETS.iter().any(|(p, _, _)| p.ends_with(".css"));
        let (html_bytes, _) = assets::lookup("/").expect("/ must serve index.html");
        let html = std::str::from_utf8(html_bytes).unwrap();
        assert!(has_js, "bundle must ship at least one JS chunk");
        assert!(
            !has_standalone_css,
            "CSS should be inlined into index.html, not shipped as a separate chunk"
        );
        assert!(
            html.contains("<style>") && html.contains(":root"),
            "index.html must carry the inlined stylesheet"
        );
    }

    // JS-side regression checks live in
    // crates/dyson/src/controller/http/web/src/__tests__/regression.test.js
    // and run under vitest via `npm test`.  build.rs chains `npm run
    // build` into the cargo build, so a failing frontend test fails
    // `cargo build` too.
}
