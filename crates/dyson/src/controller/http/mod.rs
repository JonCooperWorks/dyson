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

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::auth::{Auth, DangerousNoAuth, HashedBearerAuth, OidcAuth};
use crate::chat_history::{ChatHistory, create_chat_history};
use crate::config::{ControllerConfig, Settings};
use crate::error::DysonError;
use crate::feedback::FeedbackStore;
use crate::util::resolve_tilde;

use super::{AgentMode, ClientRegistry, Controller, Output, build_agent};

mod assets;
mod config;
mod output;
mod responses;
mod routes;
mod state;
mod stores;
mod wire;

use config::{HttpAuthConfig, HttpControllerConfigRaw, is_loopback_bind};
pub use state::HttpState;
use state::ChatHandle;
use wire::AuthMode;

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

        // DNS-rebinding gate: only matters when the bind is loopback
        // AND auth is `DangerousNoAuth`.  Any other shape (bearer /
        // OIDC, or any non-loopback bind) trusts the operator's
        // routing — turning the gate on there would 421 every reverse-
        // proxy deployment whose public Host doesn't match `127.0.0.1`.
        let loopback_only_host_check =
            is_loopback_bind(&self.bind) && matches!(auth_mode, AuthMode::None);
        let state = Arc::new(HttpState::new(
            settings.clone(),
            Arc::clone(registry),
            history.clone(),
            feedback.clone(),
            Arc::clone(&auth),
            auth_mode,
            config_path,
            loopback_only_host_check,
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
                            Ok(msgs) => routes::conversations::first_user_text(&msgs)
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
    // Cap simultaneous in-flight connections so a misbehaving client
    // (or a runaway test) can't exhaust file descriptors / RAM.  1024
    // is generous for a single-operator deployment but small enough
    // that a panic-restart can recover quickly.  An owned permit rides
    // into each spawned task and drops on connection close, freeing
    // the slot — no manual release path to forget.
    let conn_limit = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CONNS));
    loop {
        // Acquire BEFORE accept so the OS keeps the SYN queue at the
        // kernel layer rather than letting userspace queue grow.
        let permit = match Arc::clone(&conn_limit).acquire_owned().await {
            Ok(p) => p,
            Err(_) => return Ok(()), // Semaphore closed → controller shutting down.
        };
        let (stream, _addr) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "http accept error");
                drop(permit);
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let _permit = permit; // Held until connection close.
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req| {
                let state = Arc::clone(&state);
                async move { Ok::<_, Infallible>(routes::dispatch(req, state).await) }
            });
            if let Err(e) = http1::Builder::new().serve_connection(io, svc).await {
                tracing::debug!(error = %e, "http connection ended");
            }
        });
    }
}

/// Per-controller concurrent-connection ceiling.  Internal limit only —
/// no external visibility / config surface.
const MAX_CONCURRENT_CONNS: usize = 1024;

// Test-only helpers that integration tests call into.  Always compiled
// (cfg(test) is per-crate; integration tests can't see it) but
// `#[doc(hidden)]` on the module keeps it out of the public docs.
#[doc(hidden)]
pub mod test_helpers;

// ---------------------------------------------------------------------------
// Unit tests — pure helpers.  End-to-end HTTP round-trips live in
// `crates/dyson/tests/http_controller.rs` so they can bind a port.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
