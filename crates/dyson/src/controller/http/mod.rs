// ===========================================================================
// HTTP controller — web UI + JSON API + SSE event stream.
//
// Hosts the prototype at `/` (static files from a webroot directory) and
// exposes a small JSON API plus per-conversation Server-Sent Events:
//
//   GET  /                          → prototype.html
//   GET  /styles/*, /js/*, /components/*  → static assets
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

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Bytes, Frame};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::agent::Agent;
use crate::auth::{Auth, BearerTokenAuth, DangerousNoAuth};
use crate::chat_history::{ChatHistory, create_chat_history};
use crate::config::{ControllerConfig, Settings};
use crate::error::DysonError;
use crate::feedback::{FeedbackEntry, FeedbackRating, FeedbackStore};
use crate::message::{ContentBlock, Message, Role};
use crate::tool::view::ToolView;
use crate::tool::{CheckpointEvent, ToolOutput};
use crate::util::resolve_tilde;

use super::{AgentMode, ClientRegistry, Controller, Output, build_agent};

mod assets;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct HttpControllerConfigRaw {
    /// Address to bind, e.g. "127.0.0.1:7878".  Loopback-only by default
    /// because there is no inbound auth.
    #[serde(default = "default_bind")]
    bind: String,

    /// Optional override: serve static assets from this directory instead
    /// of the bundled-in prototype.  Useful when iterating on the UI
    /// without a recompile — point at
    /// `crates/dyson/src/controller/http/web`.  When unset (the default)
    /// the controller serves the prototype embedded in the dyson binary.
    #[serde(default)]
    webroot: Option<String>,

    /// Inbound authentication mechanism.  Optional on a loopback bind
    /// (127.0.0.1 / ::1) — the loopback assumption is a single trusted
    /// operator, so a missing field defaults to `DangerousNoAuth` there.
    /// On any other bind the field is required: omitting it refuses to
    /// start the controller so you can't silently expose an
    /// unauthenticated endpoint.
    #[serde(default)]
    auth: Option<HttpAuthConfig>,
}

/// Which inbound auth mechanism guards the HTTP API.
///
/// `DangerousNoAuth` is the explicit opt-in to an unauthenticated
/// endpoint — the controller still starts, but logs a loud warning.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HttpAuthConfig {
    /// No authentication.  Every request is accepted as `anonymous`.
    DangerousNoAuth,
    /// `Authorization: Bearer <token>` validated against a shared secret.
    /// `token` flows through the secret-resolver pipeline, so the config
    /// value may be either a literal string or a `{ resolver, name }`
    /// reference resolved before this struct is parsed.
    Bearer { token: String },
}

fn default_bind() -> String {
    "127.0.0.1:7878".to_string()
}

/// True when `bind` resolves to a loopback address (`127.0.0.0/8` or
/// `::1`).  Used to gate the `auth`-field default: the loopback threat
/// model is a single trusted operator, so `DangerousNoAuth` is fine
/// there; any other bind must name a mechanism explicitly.
///
/// `localhost` is intentionally NOT treated as loopback without a DNS
/// lookup — if an operator writes `localhost:7878` they're trusting
/// `/etc/hosts`, which is a different story; safer to force them to be
/// explicit.  `0.0.0.0` / `::` are NOT loopback, which is the whole
/// point.
fn is_loopback_bind(bind: &str) -> bool {
    bind.parse::<std::net::SocketAddr>()
        .map(|addr| addr.ip().is_loopback())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ConversationDto {
    id: String,
    title: String,
    /// `true` while a turn is currently executing for this chat.
    live: bool,
}

#[derive(Serialize)]
struct MessageDto {
    role: String,
    blocks: Vec<BlockDto>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BlockDto {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// Reference to an artefact rendered in the Artefacts tab.  The body
    /// lives in `HttpState.artefacts` and is fetched via
    /// `/api/artefacts/<id>` — not inlined here to keep history payloads
    /// small when a chat has produced multiple long reports.
    Artefact {
        id: String,
        kind: crate::message::ArtefactKind,
        title: String,
        /// `/api/artefacts/<id>` — lets the chip's "open" link work
        /// without the frontend having to reconstruct the URL.
        url: String,
        bytes: usize,
        /// The originating tool call, when known.  The client uses
        /// this to hydrate an image-kind artefact into the matching
        /// `image_generate` tool panel on chat reload.
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        /// Optional structured metadata — for image artefacts this
        /// carries `file_url` so the reader and the tool panel can
        /// render the image without a second round-trip.
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
}

#[derive(Deserialize)]
struct CreateChatBody {
    title: Option<String>,
    /// Chat id whose on-disk transcript should be rotated (archived)
    /// before the new conversation is minted.  The web sidebar's
    /// "+ New Conversation" button passes the currently-active chat id
    /// so starting a fresh chat also preserves the prior one as a
    /// dated archive — same shape Telegram gets on `/clear`.
    #[serde(default)]
    rotate_previous: Option<String>,
}

#[derive(Deserialize)]
struct TurnBody {
    prompt: String,
    /// Optional file attachments — base64-encoded bytes plus MIME type
    /// and original filename.  Resolved to multimodal `ContentBlock`s
    /// by `media::resolve_attachment` in `Agent::run_with_attachments`,
    /// the same path Telegram uses for photos / voice notes / docs.
    #[serde(default)]
    attachments: Vec<AttachmentDto>,
}

#[derive(Deserialize)]
struct AttachmentDto {
    /// MIME type — drives the resolver (image/* → resize, audio/* →
    /// transcribe, application/pdf → extract, text-like → wrap).
    mime_type: String,
    /// Original filename, if available.  Surfaces in the prompt so the
    /// model knows which file it is looking at.
    #[serde(default)]
    name: Option<String>,
    /// Base64-encoded bytes, NO data-URL prefix (`data:image/png;base64,`
    /// must be stripped client-side).
    data_base64: String,
}

/// Maximum total request body for `POST /turn`.  Big enough for a
/// photo or a small PDF; refuses anything that would require streaming
/// uploads (which the controller doesn't do — body is buffered into
/// memory before deserialize).
const MAX_TURN_BODY: usize = 25 * 1024 * 1024;

/// Events streamed over SSE for one conversation.
///
/// Keep these stable — the prototype's bridge.js parses them.  `view` on
/// `tool_result` is the typed payload that the right-rail panel renders
/// natively (terminal / diff / sbom / taint / read).  Tools without a
/// view leave it `None` and the panel falls back to plain text.
#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SseEvent {
    Text {
        delta: String,
    },
    /// A fragment of the model's extended-thinking / reasoning stream.
    /// Rendered in a dedicated right-rail panel — see `ThinkingPanel`
    /// in the web UI.  Arrives before any `text` event on turns where
    /// the model reasons before answering.
    Thinking {
        delta: String,
    },
    ToolStart {
        id: String,
        name: String,
    },
    ToolResult {
        content: String,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        view: Option<ToolView>,
    },
    Checkpoint {
        text: String,
    },
    /// An agent-produced file the UI can preview/download — points at
    /// `/api/files/<id>` served from the controller's in-memory file
    /// store.  `inline_image` is `true` for images so the UI can render
    /// `<img>` directly instead of a download link.
    File {
        name: String,
        mime_type: String,
        url: String,
        inline_image: bool,
    },
    /// An agent-produced artefact (e.g. a security-review report) ready
    /// for full-page markdown rendering.  The body is served at
    /// `/api/artefacts/<id>` and the metadata list at
    /// `/api/conversations/<chat>/artefacts`.
    Artefact {
        id: String,
        kind: crate::message::ArtefactKind,
        title: String,
        url: String,
        bytes: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
    LlmError {
        message: String,
    },
    Done,
}

#[derive(Serialize)]
struct ProviderDto {
    id: String,
    name: String,
    /// All models configured for this provider in dyson.json, plus any
    /// added via POST /api/providers/:id/models during this session.
    models: Vec<String>,
    /// Currently-active model name for this provider (the agent-level
    /// `model` setting when this provider is the default; otherwise the
    /// first configured model).
    active_model: String,
    /// `true` if this is the default provider configured in dyson.json.
    active: bool,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// In-memory store for agent-produced files.  Keyed by short id;
/// FIFO eviction once we exceed the cap so a long-running session
/// doesn't grow without bound.  Files are bytes + mime type; the
/// original filename is part of the SSE event the UI shows.
#[derive(Default)]
struct FileStore {
    items: HashMap<String, FileEntry>,
    order: std::collections::VecDeque<String>,
}

struct FileEntry {
    bytes: Vec<u8>,
    mime: String,
    name: String,
}

impl FileStore {
    /// In-memory cap — beyond this, oldest entries are evicted from
    /// the hot cache.  When disk persistence is enabled the bytes stay
    /// addressable on disk, so FIFO eviction is purely a memory cap.
    const MAX_FILES: usize = 64;

    fn put(&mut self, id: String, entry: FileEntry) {
        while self.order.len() >= Self::MAX_FILES {
            if let Some(old) = self.order.pop_front() {
                self.items.remove(&old);
            }
        }
        self.order.push_back(id.clone());
        self.items.insert(id, entry);
    }

    /// Read a persisted file from disk.  Returns `None` if the entry
    /// is missing or unreadable.  Called by `get_file` when the
    /// in-memory cache has evicted the id.
    fn load_from_disk(data_dir: &std::path::Path, id: &str) -> Option<FileEntry> {
        let sub = data_dir.join("files");
        let meta_path = sub.join(format!("{id}.meta.json"));
        let bytes_path = sub.join(format!("{id}.bin"));
        let meta_txt = std::fs::read_to_string(&meta_path).ok()?;
        let meta: serde_json::Value = serde_json::from_str(&meta_txt).ok()?;
        let bytes = std::fs::read(&bytes_path).ok()?;
        Some(FileEntry {
            bytes,
            mime: meta.get("mime").and_then(|v| v.as_str()).unwrap_or("application/octet-stream").to_string(),
            name: meta.get("name").and_then(|v| v.as_str()).unwrap_or("file").to_string(),
        })
    }

    /// Write an entry to disk so it survives controller restarts and
    /// is visible from every browser profile pointed at this instance.
    /// Best-effort: disk errors are logged but don't fail the tool
    /// call (the in-memory cache still serves the request).
    fn persist_static(data_dir: &std::path::Path, id: &str, entry: &FileEntry) {
        let sub = data_dir.join("files");
        if let Err(e) = std::fs::create_dir_all(&sub) {
            tracing::warn!(error = %e, "failed to create files dir");
            return;
        }
        let bytes_path = sub.join(format!("{id}.bin"));
        let meta_path = sub.join(format!("{id}.meta.json"));
        if let Err(e) = std::fs::write(&bytes_path, &entry.bytes) {
            tracing::warn!(error = %e, id, "failed to persist file bytes");
            return;
        }
        let meta = serde_json::json!({
            "mime": entry.mime,
            "name": entry.name,
        });
        if let Err(e) = std::fs::write(&meta_path, meta.to_string()) {
            tracing::warn!(error = %e, id, "failed to persist file metadata");
        }
    }

    /// On controller startup, scan the persistence dir for existing
    /// files and populate the in-memory index with just enough to serve
    /// them.  Bytes aren't loaded here — `get_file` hydrates on demand.
    /// Returns the largest numeric id seen so the controller's monotonic
    /// counter resumes above any pre-existing entry.
    fn hydrate_from_disk(&mut self, data_dir: &std::path::Path) -> u64 {
        let sub = data_dir.join("files");
        let entries = match std::fs::read_dir(&sub) {
            Ok(e) => e,
            Err(_) => return 0,
        };
        let mut max_n: u64 = 0;
        for e in entries.flatten() {
            let name = match e.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if !name.ends_with(".meta.json") { continue; }
            let id = name.trim_end_matches(".meta.json").to_string();
            // Don't eagerly load bytes — register the id in the LRU
            // so listing works; get_file pulls bytes on first hit.
            // We stash a sentinel FileEntry with empty bytes to mark
            // the slot; load_from_disk replaces it when needed.
            // But to avoid serving zero-byte files, we skip
            // registration and let get_file lazy-load instead.  Just
            // track max id.
            if let Some(rest) = id.strip_prefix('f')
                && let Ok(n) = rest.parse::<u64>()
            {
                max_n = max_n.max(n);
            }
        }
        max_n
    }
}

/// In-memory store for agent-produced artefacts (security-review
/// reports, etc.).  Mirrors `FileStore` but stores the markdown
/// body as a `String` and keeps the per-chat index, kind, title and
/// metadata alongside each entry so the Artefacts view can list
/// everything without downloading bodies.  FIFO eviction keeps memory
/// bounded.
#[derive(Default)]
struct ArtefactStore {
    items: HashMap<String, ArtefactEntry>,
    order: std::collections::VecDeque<String>,
}

struct ArtefactEntry {
    /// Chat this artefact belongs to — used to filter
    /// `/api/conversations/<chat>/artefacts`.
    chat_id: String,
    kind: crate::message::ArtefactKind,
    title: String,
    content: String,
    mime_type: String,
    metadata: Option<serde_json::Value>,
    /// The tool call that produced this artefact, when known.  Lets
    /// the UI wire image artefacts back to the tool panel on chat
    /// reload — without this, a refreshed page shows `image_generate`
    /// as a text-only tool panel with the image orphaned in chat.
    tool_use_id: Option<String>,
    /// UNIX seconds at emission.  UI sorts the list by this.
    created_at: u64,
}

impl ArtefactStore {
    /// In-memory cap — beyond this, oldest are evicted from the hot
    /// cache.  When disk persistence is enabled, the markdown stays
    /// addressable on disk so FIFO eviction is purely a memory cap.
    const MAX_ARTEFACTS: usize = 32;

    fn put(&mut self, id: String, entry: ArtefactEntry) {
        while self.order.len() >= Self::MAX_ARTEFACTS {
            if let Some(old) = self.order.pop_front() {
                self.items.remove(&old);
            }
        }
        self.order.push_back(id.clone());
        self.items.insert(id, entry);
    }

    /// Best-effort write-through to disk.  Two files per artefact:
    /// `<id>.body` (raw content) and `<id>.meta.json` (kind, title,
    /// chat id, mime, created_at, optional metadata blob).
    fn persist_static(data_dir: &std::path::Path, id: &str, entry: &ArtefactEntry) {
        let sub = data_dir.join("artefacts");
        if let Err(e) = std::fs::create_dir_all(&sub) {
            tracing::warn!(error = %e, "failed to create artefacts dir");
            return;
        }
        let body_path = sub.join(format!("{id}.body"));
        let meta_path = sub.join(format!("{id}.meta.json"));
        if let Err(e) = std::fs::write(&body_path, &entry.content) {
            tracing::warn!(error = %e, id, "failed to persist artefact body");
            return;
        }
        let kind_str = serde_json::to_value(entry.kind)
            .unwrap_or_else(|_| serde_json::Value::String("other".to_string()));
        let meta = serde_json::json!({
            "chat_id": entry.chat_id,
            "kind": kind_str,
            "title": entry.title,
            "mime_type": entry.mime_type,
            "created_at": entry.created_at,
            "metadata": entry.metadata,
            "tool_use_id": entry.tool_use_id,
        });
        if let Err(e) = std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap_or_default()) {
            tracing::warn!(error = %e, id, "failed to persist artefact metadata");
        }
    }

    /// Load a persisted artefact (meta + body) from disk.  Returns
    /// `None` if the entry is missing or malformed.
    fn load_from_disk(data_dir: &std::path::Path, id: &str) -> Option<ArtefactEntry> {
        let sub = data_dir.join("artefacts");
        let body = std::fs::read_to_string(sub.join(format!("{id}.body"))).ok()?;
        let meta_txt = std::fs::read_to_string(sub.join(format!("{id}.meta.json"))).ok()?;
        let meta: serde_json::Value = serde_json::from_str(&meta_txt).ok()?;
        let kind: crate::message::ArtefactKind = meta
            .get("kind")
            .and_then(|k| serde_json::from_value(k.clone()).ok())
            .unwrap_or(crate::message::ArtefactKind::Other);
        Some(ArtefactEntry {
            chat_id: meta.get("chat_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            kind,
            title: meta.get("title").and_then(|v| v.as_str()).unwrap_or("Artefact").to_string(),
            content: body,
            mime_type: meta.get("mime_type").and_then(|v| v.as_str()).unwrap_or("text/markdown").to_string(),
            metadata: meta.get("metadata").cloned().filter(|v| !v.is_null()),
            tool_use_id: meta.get("tool_use_id").and_then(|v| v.as_str()).map(|s| s.to_string()),
            created_at: meta.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0),
        })
    }

    /// Scan the persistence dir on startup and populate the in-memory
    /// index so the list endpoint returns everything immediately.
    /// Returns the largest numeric id seen.
    fn hydrate_from_disk(&mut self, data_dir: &std::path::Path) -> u64 {
        let sub = data_dir.join("artefacts");
        let entries = match std::fs::read_dir(&sub) {
            Ok(e) => e,
            Err(_) => return 0,
        };
        let mut ids: Vec<String> = Vec::new();
        for e in entries.flatten() {
            let name = match e.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if !name.ends_with(".meta.json") { continue; }
            let id = name.trim_end_matches(".meta.json").to_string();
            ids.push(id);
        }
        // Sort by numeric id so `order` mirrors creation order.
        ids.sort_by_key(|s| {
            s.strip_prefix('a')
                .and_then(|r| r.parse::<u64>().ok())
                .unwrap_or(0)
        });
        let mut max_n: u64 = 0;
        for id in ids {
            if let Some(rest) = id.strip_prefix('a')
                && let Ok(n) = rest.parse::<u64>()
            {
                max_n = max_n.max(n);
            }
            if let Some(entry) = Self::load_from_disk(data_dir, &id) {
                self.put(id, entry);
            }
        }
        max_n
    }
}

/// Per-chat handle.  Agent built lazily on first turn so that listing chats
/// or creating an empty one is cheap.
struct ChatHandle {
    title: String,
    /// Agent — `None` until first turn, then populated.  Behind tokio Mutex
    /// because `Agent::run` requires `&mut self` and turns are serialised
    /// per chat.
    agent: Mutex<Option<Agent>>,
    /// Broadcast channel for SSE subscribers.  Capacity is generous; a slow
    /// subscriber that lags will see "lag" gaps but nothing else breaks.
    events: broadcast::Sender<SseEvent>,
    /// Cancellation token shared with the running turn (if any).
    cancel: Mutex<Option<CancellationToken>>,
    /// `true` while a turn is in flight.
    busy: std::sync::atomic::AtomicBool,
}

impl ChatHandle {
    fn new(title: String) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            title,
            agent: Mutex::new(None),
            events: tx,
            cancel: Mutex::new(None),
            busy: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

/// Controller-wide state.  `pub` only so `test_helpers::build_state`
/// can return an `Arc<HttpState>` from outside the crate's `tests/`
/// directory; the struct's fields stay private and the type is hidden
/// from public docs.
#[doc(hidden)]
pub struct HttpState {
    settings: Settings,
    registry: Arc<ClientRegistry>,
    /// Inbound auth guard.  Every `/api/*` request is validated against
    /// this before `dispatch` routes it.  See `HttpAuthConfig`.
    auth: Arc<dyn Auth>,
    /// `Some` when the controller was configured with `webroot: "..."` —
    /// serve from disk for live-edit dev.  `None` means use the bundled
    /// embedded prototype.
    webroot: Option<PathBuf>,
    /// Persistent ChatHistory if configured in `dyson.json`.  `None`
    /// means in-memory only.
    history: Option<Arc<dyn ChatHistory>>,
    /// Per-turn rating store, sharing the same directory as ChatHistory
    /// so feedback survives across sessions and is portable to the same
    /// place Telegram writes its ratings.
    feedback: Option<Arc<FeedbackStore>>,
    /// In-memory store for agent-produced files (image_generate output,
    /// exploit_builder PoCs, etc.).  The agent calls
    /// `Output::send_file(path)`; we slurp the bytes, mint an id, push
    /// an SSE `file` event with `/api/files/<id>`, and serve it on GET.
    /// Shared with each per-turn `SseOutput` via Arc clone.  Uses
    /// `std::sync::Mutex` because Output trait methods are sync and the
    /// critical section is just a HashMap insert / get — fast enough
    /// to hold across the async boundary in `get_file`.
    files: Arc<std::sync::Mutex<FileStore>>,
    /// Monotonic id source for entries in `files`.  Atomic so multiple
    /// concurrent turns can mint without locking the store.
    file_id: Arc<std::sync::atomic::AtomicU64>,
    /// In-memory store for agent-produced artefacts (security-review
    /// reports, etc.).  The HTTP controller stamps each artefact with
    /// the chat id on emission so `/api/conversations/<id>/artefacts`
    /// can filter; the body is served from `/api/artefacts/<id>`.
    artefacts: Arc<std::sync::Mutex<ArtefactStore>>,
    artefact_id: Arc<std::sync::atomic::AtomicU64>,
    /// Root directory for on-disk persistence of agent-produced files
    /// and artefacts.  Derived from `chat_history.connection_string`
    /// when that backend is in use — this way files/artefacts survive
    /// controller restarts and are reachable from every browser
    /// profile pointed at this instance.  `None` means memory-only
    /// (the FIFO eviction still bounds memory).
    data_dir: Option<PathBuf>,
    chats: Mutex<HashMap<String, Arc<ChatHandle>>>,
    /// Insertion order so the UI can render a stable list.
    order: Mutex<Vec<String>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl HttpState {
    fn new(
        settings: Settings,
        registry: Arc<ClientRegistry>,
        webroot: Option<PathBuf>,
        history: Option<Arc<dyn ChatHistory>>,
        feedback: Option<Arc<FeedbackStore>>,
        auth: Arc<dyn Auth>,
    ) -> Self {
        // Piggy-back on the ChatHistory directory so artefacts / files /
        // chats / ratings all live in one place on disk.  Memory-only
        // deployments (no chat_history backend) stay in memory.
        let data_dir = if history.is_some() {
            Some(resolve_tilde(
                settings.chat_history.connection_string.expose(),
            ))
        } else {
            None
        };

        let mut files = FileStore::default();
        let mut artefacts = ArtefactStore::default();
        let mut file_next: u64 = 1;
        let mut artefact_next: u64 = 1;
        if let Some(dir) = data_dir.as_ref() {
            file_next = files.hydrate_from_disk(dir).saturating_add(1);
            artefact_next = artefacts.hydrate_from_disk(dir).saturating_add(1);
        }

        Self {
            settings,
            registry,
            auth,
            webroot,
            history,
            feedback,
            files: Arc::new(std::sync::Mutex::new(files)),
            file_id: Arc::new(std::sync::atomic::AtomicU64::new(file_next)),
            artefacts: Arc::new(std::sync::Mutex::new(artefacts)),
            artefact_id: Arc::new(std::sync::atomic::AtomicU64::new(artefact_next)),
            data_dir,
            chats: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Mint a fresh chat id that doesn't collide with any existing chat
    /// (in-memory or on disk).  We use `c-N` and bump until unused.
    async fn mint_id(&self) -> String {
        loop {
            let n = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let id = format!("c-{n:04}");
            let chats = self.chats.lock().await;
            if chats.contains_key(&id) {
                continue;
            }
            return id;
        }
    }

    /// Test hook — returns the stored artefact body for `id` if the
    /// store has it loaded (memory cache or freshly rehydrated from
    /// disk).  Used by the integration test for the "disk-backed
    /// rehydration" scenario (see
    /// `tests/http_controller.rs::agent_artefact_round_trips_*`).
    #[doc(hidden)]
    pub fn artefacts_for_test(&self, id: &str) -> Option<String> {
        if let Ok(s) = self.artefacts.lock()
            && let Some(entry) = s.items.get(id)
        {
            return Some(entry.content.clone());
        }
        // Fall through to disk — simulates the first hit after a
        // restart where the in-memory index may not be populated.
        self.data_dir
            .as_ref()
            .and_then(|d| ArtefactStore::load_from_disk(d, id))
            .map(|e| e.content)
    }

    /// Test hook — returns the stored file bytes for `id` via the same
    /// memory-then-disk lookup chain that `GET /api/files/:id` uses.
    /// Lets the refresh-regression test assert bytes survive controller
    /// restarts.
    #[doc(hidden)]
    pub fn file_bytes_for_test(&self, id: &str) -> Option<Vec<u8>> {
        if let Ok(s) = self.files.lock()
            && let Some(entry) = s.items.get(id)
        {
            return Some(entry.bytes.clone());
        }
        self.data_dir
            .as_ref()
            .and_then(|d| FileStore::load_from_disk(d, id))
            .map(|e| e.bytes)
    }

    /// Test hook — returns the `tool_use_id` stamped on an artefact
    /// entry, if one is set.  Used by the regression test that
    /// guarantees image_generate's image correlates back to its tool
    /// panel on refresh.
    #[doc(hidden)]
    pub fn artefact_tool_use_id_for_test(&self, id: &str) -> Option<String> {
        let s = match self.artefacts.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        s.items.get(id).and_then(|e| e.tool_use_id.clone())
    }
}

// ---------------------------------------------------------------------------
// HttpController
// ---------------------------------------------------------------------------

pub struct HttpController {
    bind: String,
    /// Disk webroot override, or `None` to use the embedded prototype.
    webroot: Option<PathBuf>,
    /// Inbound auth.  Built once from `HttpAuthConfig` and shared with
    /// every request handler via `HttpState`.
    auth: Arc<dyn Auth>,
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
        let auth: Arc<dyn Auth> = match auth_config {
            HttpAuthConfig::DangerousNoAuth => Arc::new(DangerousNoAuth),
            HttpAuthConfig::Bearer { token } => {
                if token.is_empty() {
                    tracing::error!("http controller: bearer auth configured with empty token");
                    return None;
                }
                Arc::new(BearerTokenAuth::new(token))
            }
        };
        Some(Self {
            bind: raw.bind,
            webroot: raw.webroot.map(PathBuf::from),
            auth,
        })
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

        let state = Arc::new(HttpState::new(
            settings.clone(),
            Arc::clone(registry),
            self.webroot.clone(),
            history.clone(),
            feedback.clone(),
            Arc::clone(&self.auth),
        ));

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

        let webroot_display = match self.webroot.as_ref() {
            Some(p) => p.display().to_string(),
            None => "<embedded>".into(),
        };
        tracing::info!(
            bind = %self.bind,
            webroot = %webroot_display,
            "HTTP controller listening — open http://{} in a browser",
            self.bind,
        );

        // Probe the configured auth to log which mechanism is active.
        // DangerousNoAuth is the only variant that validates an empty
        // HeaderMap successfully; treat that success as the loud warning
        // signal.  Bearer and anything else falls through to the info
        // branch.
        let empty_headers = hyper::HeaderMap::new();
        match self.auth.validate_request(&empty_headers).await {
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
        webroot: Option<PathBuf>,
        history: Option<Arc<dyn ChatHistory>>,
        feedback: Option<Arc<FeedbackStore>>,
        auth: Arc<dyn Auth>,
    ) -> Arc<HttpState> {
        Arc::new(HttpState::new(
            settings, registry, webroot, history, feedback, auth,
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

type Resp = Response<BoxBody<Bytes, Infallible>>;

async fn dispatch(req: Request<hyper::body::Incoming>, state: Arc<HttpState>) -> Resp {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // Enforce inbound auth on every API route.  Static-shell assets
    // (`/`, `/styles/*`, `/js/*`, `/components/*`) are exempt so the UI
    // can load before the browser has presented its credential.
    if path.starts_with("/api/") && state.auth.validate_request(req.headers()).await.is_err() {
        return unauthorized();
    }

    // API routes first.
    if path == "/api/conversations" && method == Method::GET {
        return list_conversations(&state).await;
    }
    if path == "/api/conversations" && method == Method::POST {
        return create_conversation(req, &state).await;
    }
    if path == "/api/providers" && method == Method::GET {
        return list_providers(&state);
    }
    if path == "/api/mind" && method == Method::GET {
        return get_mind(&state).await;
    }
    if path == "/api/mind/file" && method == Method::GET {
        return get_mind_file(&state, req.uri().query().unwrap_or("")).await;
    }
    if path == "/api/mind/file" && method == Method::POST {
        return post_mind_file(req, &state).await;
    }
    if path == "/api/activity" && method == Method::GET {
        return get_activity();
    }
    if path == "/api/model" && method == Method::POST {
        return post_model(req, Arc::clone(&state)).await;
    }
    if let Some(id) = path.strip_prefix("/api/files/") {
        if method == Method::GET {
            return get_file(&state, id).await;
        }
        return method_not_allowed();
    }
    if let Some(id) = path.strip_prefix("/api/artefacts/") {
        if method == Method::GET {
            return get_artefact(&state, id).await;
        }
        return method_not_allowed();
    }
    if let Some(id) = path.strip_prefix("/api/conversations/") {
        if let Some((id, rest)) = split_once(id, '/') {
            match (method.clone(), rest) {
                (Method::POST, "turn") => return post_turn(req, Arc::clone(&state), id).await,
                (Method::POST, "cancel") => return post_cancel(&state, id).await,
                (Method::GET, "events") => return sse_events(&state, id).await,
                (Method::GET, "feedback") => return get_feedback(&state, id).await,
                (Method::POST, "feedback") => return post_feedback(req, &state, id).await,
                (Method::GET, "artefacts") => return list_artefacts(&state, id).await,
                _ => return not_found(),
            }
        } else if method == Method::GET {
            return get_conversation(&state, id).await;
        } else {
            return method_not_allowed();
        }
    }

    // Static files from webroot.
    if method == Method::GET {
        return serve_static(&state, &path).await;
    }

    method_not_allowed()
}

fn split_once(s: &str, c: char) -> Option<(&str, &str)> {
    s.find(c).map(|i| (&s[..i], &s[i + 1..]))
}

// ---------------------------------------------------------------------------
// API: conversations
// ---------------------------------------------------------------------------

async fn list_conversations(state: &HttpState) -> Resp {
    let order = state.order.lock().await.clone();
    let chats = state.chats.lock().await;
    let mut dtos = Vec::with_capacity(order.len());
    for id in order.iter() {
        if let Some(h) = chats.get(id) {
            dtos.push(ConversationDto {
                id: id.clone(),
                title: h.title.clone(),
                live: h.busy.load(std::sync::atomic::Ordering::Relaxed),
            });
        }
    }
    json_ok(&dtos)
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
                url: format!("/api/artefacts/{aid}"),
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
            url: format!("/api/artefacts/{id}"),
            bytes: 0,
            tool_use_id: None,
            metadata: None,
        },
        // Fallback: treat image/document as a text marker so the wire
        // protocol stays simple.
        _ => BlockDto::Text {
            text: "[non-text content]".to_string(),
        },
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

    let settings = state.settings.clone();
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
            let client = registry.get_default();
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
        agent.set_cancellation_token(cancel);

        // Branch on attachments: with attachments, dispatch through
        // run_with_attachments so images/audio/PDF are resolved into
        // multimodal ContentBlocks (same path Telegram takes).
        let result = if attachments.is_empty() {
            agent.run(&prompt, &mut output).await
        } else {
            agent.run_with_attachments(&prompt, attachments, &mut output).await
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
    // Sort the active provider first so /api/providers[0] is the source of
    // truth for the UI's "active model" label.
    let active = super::active_provider_name(&state.settings);
    let mut dtos: Vec<ProviderDto> = state
        .settings
        .providers
        .iter()
        .map(|(id, pc)| {
            let is_active = active.as_deref() == Some(id.as_str());
            let active_model = if is_active {
                state.settings.agent.model.clone()
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

#[derive(Deserialize)]
struct FeedbackBody {
    turn_index: usize,
    /// Emoji to map to a rating (matches the Telegram controller's set).
    /// When omitted or empty, removes any existing feedback for this turn.
    #[serde(default)]
    emoji: Option<String>,
}

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

async fn serve_static(state: &HttpState, path: &str) -> Resp {
    if path.contains("..") {
        return not_found();
    }
    // Disk webroot wins when configured (dev mode — edit + reload without
    // recompile).  Otherwise serve from the embedded prototype.
    if let Some(webroot) = state.webroot.as_ref() {
        let rel = if path == "/" {
            "prototype.html"
        } else {
            path.trim_start_matches('/')
        };
        let full = webroot.join(rel);
        match tokio::fs::read(&full).await {
            Ok(bytes) => {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", content_type_for(rel))
                    .header("Cache-Control", "no-cache")
                    .body(boxed(Bytes::from(bytes)))
                    .unwrap();
            }
            Err(_) => return not_found(),
        }
    }
    match assets::lookup(path) {
        Some((bytes, ct)) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", ct)
            .header("Cache-Control", "no-cache")
            .body(boxed(Bytes::from_static(bytes)))
            .unwrap(),
        None => not_found(),
    }
}

/// MIME type for an agent-produced file based on extension.  Used by
/// `send_file` to label inline images vs. download attachments.
fn mime_for_extension(path: &std::path::Path) -> String {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "html" | "htm" => "text/html; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn content_type_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "jsx" => "text/babel; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------------------
// SseOutput — implements `Output` by fanning events into a broadcast channel.
// ---------------------------------------------------------------------------

struct SseOutput {
    /// Which chat this output is scoped to — stamped onto every
    /// artefact so `/api/conversations/<id>/artefacts` can filter.
    chat_id: String,
    tx: broadcast::Sender<SseEvent>,
    /// Shared file store so `send_file` can stash agent-produced bytes
    /// for the UI to fetch via `/api/files/<id>`.
    files: Arc<std::sync::Mutex<FileStore>>,
    /// Counter for synthesising file ids when the agent attaches an
    /// unnamed file.  Wraps; collisions are vanishingly unlikely
    /// inside the FileStore::MAX_FILES window.
    next_file_id: Arc<std::sync::atomic::AtomicU64>,
    /// Shared artefact store for `send_artefact`.
    artefacts: Arc<std::sync::Mutex<ArtefactStore>>,
    next_artefact_id: Arc<std::sync::atomic::AtomicU64>,
    /// Optional write-through disk directory for persistence.
    data_dir: Option<PathBuf>,
    /// Currently-executing tool's id.  Set in `tool_use_start` and
    /// carried across subsequent `send_file` / `send_artefact` calls
    /// that the agent loop triggers from the same `ToolOutput`.  The
    /// id is stamped on every file and artefact entry so the UI can
    /// wire image artefacts back to the originating tool panel on
    /// chat reload.
    current_tool_use_id: Option<String>,
}

impl SseOutput {
    fn send(&self, evt: SseEvent) {
        // Ignore receiver-count errors — there may be no subscribers right
        // now; events are still useful when one connects mid-turn (only the
        // most recent N stay buffered, that's fine for SSE semantics).
        let _ = self.tx.send(evt);
    }
}

impl Output for SseOutput {
    fn text_delta(&mut self, text: &str) -> std::result::Result<(), DysonError> {
        self.send(SseEvent::Text {
            delta: text.to_string(),
        });
        Ok(())
    }

    fn thinking_delta(&mut self, text: &str) -> std::result::Result<(), DysonError> {
        self.send(SseEvent::Thinking {
            delta: text.to_string(),
        });
        Ok(())
    }

    fn tool_use_start(&mut self, id: &str, name: &str) -> std::result::Result<(), DysonError> {
        // Remember which tool is running so any `send_file` /
        // `send_artefact` calls that follow this turn's `tool_result`
        // can stamp the same id on their FileEntry / ArtefactEntry.
        // Reset in `flush` at turn end; NOT reset in `tool_result` —
        // files are emitted AFTER the tool result per execution.rs.
        self.current_tool_use_id = Some(id.to_string());
        self.send(SseEvent::ToolStart {
            id: id.to_string(),
            name: name.to_string(),
        });
        Ok(())
    }

    fn tool_use_complete(&mut self) -> std::result::Result<(), DysonError> {
        Ok(())
    }

    fn tool_result(&mut self, output: &ToolOutput) -> std::result::Result<(), DysonError> {
        self.send(SseEvent::ToolResult {
            content: output.content.clone(),
            is_error: output.is_error,
            view: output.view.clone(),
        });
        Ok(())
    }

    fn send_file(&mut self, path: &std::path::Path) -> std::result::Result<(), DysonError> {
        // Slurp the file (size-capped to keep a runaway tool from
        // blowing memory), park it in the shared FileStore, emit an
        // SSE `file` event with the URL the UI fetches.
        const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
        match std::fs::metadata(path) {
            Ok(m) if m.len() > MAX_FILE_BYTES => {
                self.send(SseEvent::Text {
                    delta: format!(
                        "\n[file: {} too large ({} MB) — not delivered]\n",
                        path.display(), m.len() / (1024 * 1024),
                    ),
                });
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                self.send(SseEvent::Text {
                    delta: format!("\n[file: {} — stat failed: {e}]\n", path.display()),
                });
                return Ok(());
            }
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.send(SseEvent::Text {
                    delta: format!("\n[file: {} — read failed: {e}]\n", path.display()),
                });
                return Ok(());
            }
        };
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        let mime = mime_for_extension(path);
        let id = format!(
            "f{}",
            self.next_file_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let inline_image = mime.starts_with("image/");
        let url = format!("/api/files/{id}");
        let bytes_len = bytes.len();
        let entry = FileEntry { bytes, mime: mime.clone(), name: name.clone() };
        // Write-through to disk first so a controller crash between
        // the memory put and the disk write doesn't leak a dangling
        // in-memory reference that can't be rehydrated.
        if let Some(dir) = self.data_dir.as_ref() {
            FileStore::persist_static(dir, &id, &entry);
        }
        // std::sync::Mutex — blocking but the critical section is a
        // HashMap insert + a Vec push.  Negligible contention.
        if let Ok(mut s) = self.files.lock() {
            s.put(id.clone(), entry);
        }
        self.send(SseEvent::File {
            name: name.clone(),
            mime_type: mime.clone(),
            url: url.clone(),
            inline_image,
        });

        // Images are also artefacts — listing them in the Artefacts
        // tab makes a chat's generated images discoverable after the
        // original chat scroll has paged them away.  The body here is
        // the served URL (not the raw bytes); the reader notices the
        // `image/*` mime and renders with `<img>` instead of markdown.
        if inline_image {
            let artefact = crate::message::Artefact {
                id: String::new(),
                kind: crate::message::ArtefactKind::Image,
                title: name.clone(),
                content: url.clone(),
                mime_type: mime.clone(),
                metadata: Some(serde_json::json!({
                    "file_url": url,
                    "file_name": name,
                    "bytes": bytes_len,
                })),
            };
            let _ = self.send_artefact(&artefact);
        }

        Ok(())
    }

    fn checkpoint(&mut self, event: &CheckpointEvent) -> std::result::Result<(), DysonError> {
        // CheckpointEvent has no Display impl — Debug suffices for the
        // UI's progress feed in v1.  Replace with a typed event later.
        self.send(SseEvent::Checkpoint {
            text: format!("{event:?}"),
        });
        Ok(())
    }

    fn send_artefact(
        &mut self,
        artefact: &crate::message::Artefact,
    ) -> std::result::Result<(), DysonError> {
        let id = format!(
            "a{}",
            self.next_artefact_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let url = format!("/api/artefacts/{id}");
        let bytes = artefact.content.len();
        let entry = ArtefactEntry {
            chat_id: self.chat_id.clone(),
            kind: artefact.kind,
            title: artefact.title.clone(),
            content: artefact.content.clone(),
            mime_type: artefact.mime_type.clone(),
            metadata: artefact.metadata.clone(),
            tool_use_id: self.current_tool_use_id.clone(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        if let Some(dir) = self.data_dir.as_ref() {
            ArtefactStore::persist_static(dir, &id, &entry);
        }
        if let Ok(mut s) = self.artefacts.lock() {
            s.put(id.clone(), entry);
        }
        self.send(SseEvent::Artefact {
            id,
            kind: artefact.kind,
            title: artefact.title.clone(),
            url,
            bytes,
            metadata: artefact.metadata.clone(),
        });
        Ok(())
    }

    fn error(&mut self, error: &DysonError) -> std::result::Result<(), DysonError> {
        self.send(SseEvent::LlmError {
            message: error.to_string(),
        });
        Ok(())
    }

    fn flush(&mut self) -> std::result::Result<(), DysonError> {
        // End of turn — the next turn's `tool_use_start` will set a
        // new id; until then there's no "current" tool.
        self.current_tool_use_id = None;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// API: mind / activity
// ---------------------------------------------------------------------------

/// List workspace files for the Mind view.  Pulls metadata from disk
/// (size, modified-at) when the workspace lives on a real filesystem.
async fn get_mind(state: &HttpState) -> Resp {
    let ws = match crate::workspace::create_workspace(&state.settings.workspace) {
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
        "backend": state.settings.workspace.backend,
        "files": files,
    }))
}

/// Return one workspace file's content for the Mind editor pane.
async fn get_mind_file(state: &HttpState, query: &str) -> Resp {
    let path = match parse_query(query).into_iter().find(|(k, _)| k == "path") {
        Some((_, v)) => v,
        None => return bad_request("missing 'path' query parameter"),
    };
    let ws = match crate::workspace::create_workspace(&state.settings.workspace) {
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
            )
        })
    };
    let (bytes, mime, title) = match cached {
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
            format!("inline; filename=\"{}.md\"", title.replace('"', "")),
        )
        .header("Cache-Control", "no-cache")
        .body(boxed(Bytes::from(bytes)))
        .unwrap()
}

/// Wire shape for `GET /api/conversations/<chat>/artefacts`.  One entry
/// per artefact emitted for this chat, ordered newest first.  The
/// reader fetches the body separately from `/api/artefacts/<id>` so the
/// list is cheap to render even when reports are multi-KB.
#[derive(Serialize)]
struct ArtefactDto {
    id: String,
    kind: crate::message::ArtefactKind,
    title: String,
    bytes: usize,
    created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<serde_json::Value>,
}

/// List artefacts for a given chat.  Empty list if none exist yet.
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

/// Background-agents / dreams / swarm activity.  The HTTP controller
/// doesn't currently own a `BackgroundAgentRegistry` (each controller
/// has its own — coordinating one across controllers is a follow-up).
/// Returns an empty list so the prototype's Activity view doesn't 500;
/// the seed data still paints the page until cross-controller state
/// lands.
fn get_activity() -> Resp {
    json_ok(&serde_json::json!({ "lanes": [] }))
}

#[derive(Deserialize)]
struct MindWriteBody {
    path: String,
    content: String,
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
    let mut ws = match crate::workspace::create_workspace(&state.settings.workspace) {
        Ok(w) => w,
        Err(e) => return bad_request(&format!("workspace open failed: {e}")),
    };
    ws.set(&body.path, &body.content);
    if let Err(e) = ws.save() {
        return bad_request(&format!("workspace save failed: {e}"));
    }
    json_ok(&serde_json::json!({ "ok": true, "path": body.path }))
}

#[derive(Deserialize)]
struct ModelSwitchBody {
    /// Provider name from `dyson.json` providers table.
    provider: String,
    /// Optional model — defaults to the provider's first configured model.
    model: Option<String>,
    /// Optional chat to swap on.  When omitted, swaps every loaded chat.
    chat_id: Option<String>,
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
    let provider_cfg = match state.settings.providers.get(&body.provider) {
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
    json_ok(&serde_json::json!({
        "ok": true,
        "provider": body.provider,
        "model": model,
        "swapped": swapped,
    }))
}

/// Tiny URL-query parser, sufficient for `?path=foo&bar=baz`.
fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((url_decode(k), url_decode(v)))
        })
        .collect()
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn boxed(bytes: Bytes) -> BoxBody<Bytes, Infallible> {
    Full::new(bytes).boxed()
}

fn json_ok<T: Serialize>(v: &T) -> Resp {
    let bytes = serde_json::to_vec(v).unwrap_or_else(|_| b"null".to_vec());
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(boxed(Bytes::from(bytes)))
        .unwrap()
}

fn bad_request(msg: &str) -> Resp {
    let body = serde_json::json!({ "error": msg }).to_string();
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header("Content-Type", "application/json")
        .body(boxed(Bytes::from(body)))
        .unwrap()
}

fn not_found() -> Resp {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("Content-Type", "application/json")
        .body(boxed(Bytes::from_static(br#"{"error":"not found"}"#)))
        .unwrap()
}

fn method_not_allowed() -> Resp {
    Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .header("Content-Type", "application/json")
        .body(boxed(Bytes::from_static(
            br#"{"error":"method not allowed"}"#,
        )))
        .unwrap()
}

fn unauthorized() -> Resp {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("Content-Type", "application/json")
        .body(boxed(Bytes::from_static(br#"{"error":"unauthorized"}"#)))
        .unwrap()
}

async fn read_json<T: for<'de> Deserialize<'de>>(
    req: Request<hyper::body::Incoming>,
) -> std::result::Result<T, String> {
    let collected = req
        .collect()
        .await
        .map_err(|e| format!("body read: {e}"))?;
    let bytes = collected.to_bytes();
    serde_json::from_slice(&bytes).map_err(|e| format!("json parse: {e}"))
}

/// Like `read_json` but with a hard byte cap.  Used by upload-bearing
/// endpoints to refuse oversized requests after the body is read
/// (the Content-Length pre-check covers honest clients; this catches
/// chunked bodies that lie about length).
async fn read_json_capped<T: for<'de> Deserialize<'de>>(
    req: Request<hyper::body::Incoming>,
    max: usize,
) -> std::result::Result<T, String> {
    let collected = req
        .collect()
        .await
        .map_err(|e| format!("body read: {e}"))?;
    let bytes = collected.to_bytes();
    if bytes.len() > max {
        return Err(format!("body too large ({} bytes; max {max})", bytes.len()));
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("json parse: {e}"))
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

    #[test]
    fn url_decode_handles_percent_and_plus() {
        assert_eq!(url_decode("foo%20bar"), "foo bar");
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(url_decode("memory%2F2026.md"), "memory/2026.md");
        // Bad escape — pass through the literal % rather than panic.
        assert_eq!(url_decode("100%"), "100%");
    }

    #[test]
    fn parse_query_extracts_path() {
        let pairs = parse_query("path=memory%2FSOUL.md&x=1");
        assert!(pairs.iter().any(|(k, v)| k == "path" && v == "memory/SOUL.md"));
    }

    #[test]
    fn content_type_for_known_extensions() {
        assert!(content_type_for("prototype.html").starts_with("text/html"));
        assert!(content_type_for("styles/x.css").starts_with("text/css"));
        assert!(content_type_for("js/x.js").starts_with("application/javascript"));
        assert_eq!(content_type_for("x.jsx"), "text/babel; charset=utf-8");
        assert_eq!(content_type_for("x.unknown"), "application/octet-stream");
    }

    // The embedded prototype is part of the binary's contract — these
    // tests guarantee the include_bytes! table is intact and that the
    // JS/JSX surface the web UI depends on is actually shipped.

    #[test]
    fn embedded_assets_serve_root() {
        let (bytes, ct) = assets::lookup("/").expect("/ must serve prototype.html");
        assert!(ct.starts_with("text/html"));
        assert!(!bytes.is_empty());
        assert!(
            std::str::from_utf8(bytes).unwrap().contains("<div id=\"root\">"),
            "prototype.html must mount React at #root",
        );
    }

    #[test]
    fn embedded_assets_serve_every_required_file() {
        // Files the prototype's <script>/<link> tags load.  If any
        // disappears, the UI breaks silently in the browser.
        const REQUIRED: &[&str] = &[
            "/styles/tokens.css",
            "/styles/layout.css",
            "/styles/turns.css",
            "/styles/panels.css",
            "/js/data.js",
            "/js/bridge.js",
            "/components/icons.jsx",
            "/components/panels.jsx",
            "/components/turns.jsx",
            "/components/views.jsx",
            "/components/app.jsx",
        ];
        for path in REQUIRED {
            let (bytes, _) = assets::lookup(path)
                .unwrap_or_else(|| panic!("missing embedded asset {path}"));
            assert!(!bytes.is_empty(), "{path} is empty");
        }
    }

    // ----- JS-side regression checks -----
    //
    // The web UI is JSX wrapped per-script-tag by Babel-in-browser.
    // Each file is its own IIFE, so cross-file references must be
    // hung off `window` via Object.assign.  Components from views.jsx
    // are referenced from app.jsx — a missing export there produces a
    // grey screen with `<X> is not defined` in the console.
    //
    // These tests grep the embedded JS for the load-bearing patterns
    // so a deleted export or a regression of the bugs we hit while
    // building this controller surfaces in `cargo test`.

    fn jsx(path: &str) -> &'static str {
        let (bytes, _) = assets::lookup(path).expect("asset must exist");
        std::str::from_utf8(bytes).expect("asset is utf-8")
    }

    #[test]
    fn views_exports_components_app_uses() {
        // app.jsx renders <TopBar>, <LeftRail>, <RightRail>, <MindView>,
        // <ActivityView> — all defined in views.jsx.  Each must be
        // attached to window so the IIFE-wrapped app.jsx can see them.
        let v = jsx("/components/views.jsx");
        for name in ["TopBar", "LeftRail", "RightRail", "MindView", "ActivityView"] {
            assert!(
                v.contains(&format!("Object.assign(window, {{")) && v.contains(name),
                "views.jsx must export {name} on window",
            );
        }
    }

    #[test]
    fn turns_exports_only_live_components() {
        // SubagentCard and ErrorCard were deleted; they must not be in
        // the export list (referencing a deleted name throws on load).
        let t = jsx("/components/turns.jsx");
        assert!(t.contains("Object.assign(window, {"));
        for dead in ["SubagentCard", "ErrorCard"] {
            assert!(
                !t.contains(&format!(", {dead}")) && !t.contains(&format!("{{ {dead}")),
                "turns.jsx still exports deleted name {dead}",
            );
        }
        for live in ["Turn", "ToolChip", "Composer", "EmptyState", "markdown"] {
            assert!(t.contains(live), "turns.jsx must export {live}");
        }
    }

    #[test]
    fn keyboard_handler_does_not_outrun_view_ids() {
        // Regression for the "⌘4/⌘5 grey-screen" bug: the keyboard
        // handler used to map [1-5] to a hardcoded array longer than
        // the rendered <Route>s, so pressing ⌘4 set view='providers'
        // and nothing rendered.  app.jsx now uses VIEW_IDS as the
        // single source of truth, with a bounds check.
        let app = jsx("/components/app.jsx");
        assert!(
            app.contains("const VIEW_IDS"),
            "app.jsx must define VIEW_IDS as the source of truth",
        );
        assert!(
            app.contains("idx < VIEW_IDS.length"),
            "app.jsx keyboard handler must bounds-check against VIEW_IDS",
        );
        assert!(
            !app.contains("['conv','mind','activity','providers','sandbox']"),
            "app.jsx still references the deleted Providers/Sandbox views",
        );
    }

    #[test]
    fn transcript_force_scrolls_to_bottom_on_load() {
        // Regression for "chats open at the top": a long-loaded
        // transcript would render scrolled to the top because the
        // auto-scroll only fired when the user was already near the
        // bottom (and a fresh element has scrollTop=0).  app.jsx now
        // marks `session.justScrollOnNextRender` on conv switch and
        // force-scrolls the next render that has turns.
        let app = jsx("/components/app.jsx");
        assert!(
            app.contains("justScrollOnNextRender"),
            "app.jsx must mark just-loaded conversations to force-scroll",
        );
    }

    #[test]
    fn per_chat_session_state_survives_conv_switch() {
        // Regression for "moving from a chat seems to kill it" and
        // "the tool stack is not per conversation".  Per-chat state
        // (transcript, panels, ratings, in-flight EventSource) lives
        // in `sessionsRef: Map<chat_id, Session>` so switching `conv`
        // never clears prior chat state and inactive chats keep
        // streaming.  The forbidden patterns below were the bug.
        let app = jsx("/components/app.jsx");
        assert!(
            app.contains("sessionsRef") && app.contains("makeSession"),
            "app.jsx must keep a per-chat session map",
        );
        assert!(
            !app.contains("setLiveTurns([])"),
            "conv-change must NOT wipe liveTurns — that killed the chat the user just left",
        );
        // RightRail must read from the active session's panels rather
        // than a global `panels` state shared across chats.
        assert!(
            app.contains("session.panels") || app.contains("session ? session.panels"),
            "RightRail must take its panels from the active session",
        );
        // SSE must be stored on the session so it keeps streaming for
        // chats the user navigated away from.
        assert!(
            app.contains("session.es ="),
            "EventSource must be parked on the per-chat session",
        );
    }

    #[test]
    fn markdown_inline_code_does_not_leak_placeholders() {
        // Regression for the "CODE0 / CODEBLOCK_0 leaked into chat
        // output" bug.  Inline-code and fenced-code placeholders used
        // \u0000/\u0001 — control chars the DOM strips on innerHTML
        // assignment, so the literal placeholder text leaked through.
        // The fix: don't use control chars at all (inline-code is
        // tokenised by split-on-backticks, fenced uses §§ printable).
        let t = jsx("/components/turns.jsx");
        assert!(
            !t.contains("\\u0000CODEBLOCK_") && !t.contains("\\u0001CODE_"),
            "turns.jsx still uses control-char placeholders the DOM strips",
        );
        assert!(
            t.contains("split(/(`[^`]+`)/g)"),
            "inline() must tokenise on backticks rather than placeholder-substitute",
        );
    }

    #[test]
    fn file_block_renders_inline_image_or_download_link() {
        // Regression for "agent can't deliver files to the UI".  The
        // SSE `file` event must be dispatched into the transcript as
        // a `file` block; FileBlock renders inline <img> for images
        // and a download link for other MIME types.
        let t = jsx("/components/turns.jsx");
        let app = jsx("/components/app.jsx");
        let bridge = jsx("/js/bridge.js");
        assert!(t.contains("function FileBlock"), "turns.jsx must export FileBlock");
        assert!(t.contains("b.type === 'file'"), "Turn must dispatch 'file' blocks to FileBlock");
        assert!(bridge.contains("case 'file':"), "bridge.js must dispatch SSE 'file' events");
        assert!(app.contains("onFile:"), "ConversationView must wire onFile to push 'file' blocks");
    }

    #[test]
    fn composer_uses_real_file_input_not_a_fake_chip() {
        // Regression for "paperclip pretends to attach a file".  The
        // composer must mount an <input type="file"> and actually pass
        // real File objects through to bridge → backend.
        let t = jsx("/components/turns.jsx");
        assert!(
            t.contains("type=\"file\""),
            "Composer must use a real <input type=\"file\"> not a fake chip",
        );
        assert!(
            !t.contains("'screenshot.png'") || !t.contains("name: 'screenshot.png'"),
            "the hardcoded fake screenshot.png attachment must be gone",
        );
        assert!(
            t.contains("function fileToBase64"),
            "the FileReader → base64 helper must exist for upload serialisation",
        );
    }

    #[test]
    fn live_tool_ids_are_namespaced_per_chat() {
        // Two chats minting `live-1` would collide in window.DYSON_DATA.tools.
        // The fix is to prefix newly-minted tool ids with the chat id.
        let app = jsx("/components/app.jsx");
        assert!(
            app.contains("`${conv}-live-${"),
            "live tool ids must be namespaced by conv to avoid cross-chat collisions",
        );
    }
}
