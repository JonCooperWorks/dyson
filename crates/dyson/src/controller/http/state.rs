// ===========================================================================
// HTTP controller — runtime state + per-chat handle + cross-controller sink.
//
// `HttpState` is the single shared object every route reads from and
// every per-turn `SseOutput` writes into.  It owns: the live `Settings`
// snapshot (RwLock so the program-level hot-reload task can swap in
// fresh values), the inbound auth, the chat-history backend, the
// feedback store, the FIFO file/artefact stores, the activity
// registry, and the per-chat `ChatHandle`s with their broadcast
// channels.
//
// `BrowserArtefactSink` is the cross-controller hook: when a Telegram
// turn calls `Output::send_file`, the controller calls
// `HttpState::publish_file_as_artefact` which mirrors the file into
// the same store the agent's own `send_file` uses and (best-effort)
// broadcasts a live SSE event if a browser is currently subscribed.
// ===========================================================================

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::agent::Agent;
use crate::auth::Auth;
use crate::chat_history::ChatHistory;
use crate::config::Settings;
use crate::feedback::FeedbackStore;
use crate::util::resolve_tilde;

use super::ClientRegistry;
use super::stores::{
    ArtefactEntry, ArtefactStore, FileEntry, FileStore, max_file_id,
};
use super::wire::{AuthMode, SseEvent};

/// Per-chat handle.  Agent built lazily on first turn so that listing chats
/// or creating an empty one is cheap.
pub(crate) struct ChatHandle {
    pub(crate) title: String,
    /// Agent — `None` until first turn, then populated.  Behind tokio Mutex
    /// because `Agent::run` requires `&mut self` and turns are serialised
    /// per chat.
    pub(crate) agent: Mutex<Option<Agent>>,
    /// Broadcast channel for SSE subscribers.  Capacity is generous; a slow
    /// subscriber that lags will see "lag" gaps but nothing else breaks.
    pub(crate) events: broadcast::Sender<SseEvent>,
    /// Cancellation token shared with the running turn (if any).
    pub(crate) cancel: Mutex<Option<CancellationToken>>,
    /// `true` while a turn is in flight.
    pub(crate) busy: std::sync::atomic::AtomicBool,
}

impl ChatHandle {
    pub(crate) fn new(title: String) -> Self {
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
    /// Live runtime settings.  Wrapped in a RwLock so the hot-reload
    /// task spawned in `HttpController::run` can swap in a fresh
    /// snapshot when `dyson.json` changes on disk — without this, a
    /// model added to the config file would never show up in the web
    /// UI's provider list (the /api/providers endpoint used to read
    /// from a frozen startup clone).  Reads are short (usually a
    /// field lookup or a clone); no observable lock contention.
    pub(crate) settings: std::sync::RwLock<Settings>,
    pub(crate) registry: Arc<ClientRegistry>,
    /// Inbound auth guard.  Every `/api/*` request is validated against
    /// this before `dispatch` routes it.  See `HttpAuthConfig`.
    pub(crate) auth: Arc<dyn Auth>,
    /// Persistent ChatHistory if configured in `dyson.json`.  `None`
    /// means in-memory only.
    pub(crate) history: Option<Arc<dyn ChatHistory>>,
    /// Per-turn rating store, sharing the same directory as ChatHistory
    /// so feedback survives across sessions and is portable to the same
    /// place Telegram writes its ratings.
    pub(crate) feedback: Option<Arc<FeedbackStore>>,
    /// In-memory store for agent-produced files (image_generate output,
    /// exploit_builder PoCs, etc.).  The agent calls
    /// `Output::send_file(path)`; we slurp the bytes, mint an id, push
    /// an SSE `file` event with `/api/files/<id>`, and serve it on GET.
    /// Shared with each per-turn `SseOutput` via Arc clone.  Uses
    /// `std::sync::Mutex` because Output trait methods are sync and the
    /// critical section is just a HashMap insert / get — fast enough
    /// to hold across the async boundary in `get_file`.
    pub(crate) files: Arc<std::sync::Mutex<FileStore>>,
    /// Monotonic id source for entries in `files`.  Atomic so multiple
    /// concurrent turns can mint without locking the store.
    pub(crate) file_id: Arc<std::sync::atomic::AtomicU64>,
    /// In-memory store for agent-produced artefacts (security-review
    /// reports, etc.).  The HTTP controller stamps each artefact with
    /// the chat id on emission so `/api/conversations/<id>/artefacts`
    /// can filter; the body is served from `/api/artefacts/<id>`.
    pub(crate) artefacts: Arc<std::sync::Mutex<ArtefactStore>>,
    pub(crate) artefact_id: Arc<std::sync::atomic::AtomicU64>,
    /// Root directory for on-disk persistence of agent-produced files
    /// and artefacts.  Derived from `chat_history.connection_string`
    /// when that backend is in use — this way files/artefacts survive
    /// controller restarts and are reachable from every browser
    /// profile pointed at this instance.  `None` means memory-only
    /// (the FIFO eviction still bounds memory).
    pub(crate) data_dir: Option<PathBuf>,
    /// Per-chat registry of running / recently-finished tool activity
    /// (subagents today; other lanes later).  Backs `/api/activity`
    /// and the Activity tab in the web UI.  Disk-backed via
    /// `{data_dir}/{chat_id}/activity.jsonl` so entries survive
    /// controller restarts.  UI-only side channel — never feeds any
    /// LLM prompt.
    pub(crate) activity: Arc<crate::controller::ActivityRegistry>,
    pub(crate) chats: Mutex<HashMap<String, Arc<ChatHandle>>>,
    /// Insertion order so the UI can render a stable list.
    pub(crate) order: Mutex<Vec<String>>,
    pub(crate) next_id: std::sync::atomic::AtomicU64,
    /// Path to `dyson.json` resolved from `--config` / default at
    /// startup.  `None` when Dyson was launched without a config on
    /// disk (in-memory-only scenario).  Used by `post_model` to
    /// persist the operator's model choice across restarts — Telegram
    /// already does this, and without the HTTP side participating the
    /// web UI's choice got silently reverted the next time the
    /// process rebuilt an agent from settings.
    pub(crate) config_path: Option<PathBuf>,
    /// In-memory override for `(provider, model)` applied to any
    /// agent built after `post_model` has run — `state.settings` is a
    /// frozen snapshot from startup, so without this override a new
    /// conversation (and any first-use agent build) would reuse the
    /// startup model.  Cleared on process restart — the persisted
    /// `dyson.json` write is what carries the choice across restarts.
    pub(crate) runtime_model: std::sync::Mutex<Option<(String, String)>>,
    /// Public auth-mode summary the frontend needs to bootstrap an
    /// auth code flow.  Surfaced via `/api/auth/config` (unauth-
    /// enticated) and embedded in `WWW-Authenticate` on 401s when the
    /// mode is OIDC.  The real auth guard is `auth: Arc<dyn Auth>`;
    /// this is just the metadata required to drive a browser
    /// redirect, never used in the validation path.
    pub(crate) auth_mode: AuthMode,
}

/// Scan the chat directory (files + archives + artefact metadata) for
/// the highest `c-NNNN` ever used.  Ensures a new chat id never reuses
/// a slot that another record still points at — otherwise the empty
/// new chat would surface orphan artefacts filtered by the old id.
pub(crate) fn max_chat_id_n(data_dir: &std::path::Path, artefacts: &ArtefactStore) -> u64 {
    fn extract(name: &str) -> Option<u64> {
        let stem = name.strip_prefix("c-")?;
        let digits: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        digits.parse().ok()
    }

    let mut max_n: u64 = 0;
    if let Ok(iter) = std::fs::read_dir(data_dir) {
        for entry in iter.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && let Some(n) = extract(name)
            {
                max_n = max_n.max(n);
            }
        }
    }
    // Artefacts retain the owning chat_id even when the chat file has
    // been purged — rotation leaves them orphaned on disk.  Walk the
    // in-memory index (already hydrated from disk) for c-NNNN hits.
    for entry in artefacts.items.values() {
        if let Some(n) = extract(&entry.chat_id) {
            max_n = max_n.max(n);
        }
    }
    max_n
}

impl HttpState {
    pub(crate) fn new(
        settings: Settings,
        registry: Arc<ClientRegistry>,
        history: Option<Arc<dyn ChatHistory>>,
        feedback: Option<Arc<FeedbackStore>>,
        auth: Arc<dyn Auth>,
        auth_mode: AuthMode,
        config_path: Option<PathBuf>,
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

        let files = FileStore::default();
        let mut artefacts = ArtefactStore::default();
        let mut file_next: u64 = 1;
        let mut artefact_next: u64 = 1;
        let mut chat_next: u64 = 1;
        if let Some(dir) = data_dir.as_ref() {
            file_next = max_file_id(dir).saturating_add(1);
            artefact_next = artefacts.hydrate_from_disk(dir).saturating_add(1);
            chat_next = max_chat_id_n(dir, &artefacts).saturating_add(1);
        }

        let activity = Arc::new(crate::controller::ActivityRegistry::new(
            data_dir.clone(),
        ));

        Self {
            settings: std::sync::RwLock::new(settings),
            registry,
            auth,
            history,
            feedback,
            files: Arc::new(std::sync::Mutex::new(files)),
            file_id: Arc::new(std::sync::atomic::AtomicU64::new(file_next)),
            artefacts: Arc::new(std::sync::Mutex::new(artefacts)),
            artefact_id: Arc::new(std::sync::atomic::AtomicU64::new(artefact_next)),
            data_dir,
            activity,
            chats: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            next_id: std::sync::atomic::AtomicU64::new(chat_next),
            config_path,
            runtime_model: std::sync::Mutex::new(None),
            auth_mode,
        }
    }

    /// Mint a fresh chat id that doesn't collide with any existing chat
    /// (in-memory, rotated archive, or referenced by an artefact).
    /// `next_id` is primed at startup from the max `c-NNNN` ever seen
    /// on disk so freshly-minted ids never reuse a slot that still has
    /// artefact metadata tagged to it.
    pub(crate) async fn mint_id(&self) -> String {
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
        // Recover from poisoning: a previous holder that panicked still
        // left a valid `ArtefactStore` behind.  Skipping on Err would
        // silently disable this accessor for the rest of the process.
        let s = match self.artefacts.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        if let Some(entry) = s.items.get(id) {
            return Some(entry.content.clone());
        }
        drop(s);
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
        let s = match self.files.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        if let Some(entry) = s.items.get(id) {
            return Some(entry.bytes.clone());
        }
        drop(s);
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

    /// Test hook — grants raw access to the configured `ChatHistory`
    /// so integration tests can seed transcripts directly, bypassing
    /// the agent loop.  `None` when the state was built without a
    /// history backend.
    #[doc(hidden)]
    pub fn history_for_test(&self) -> Option<Arc<dyn ChatHistory>> {
        self.history.clone()
    }

    /// Test hook — swap the live settings snapshot.  Lets the
    /// hot-reload regression test verify that a config change
    /// (e.g. a new model added to a provider) propagates through
    /// `/api/providers` without restarting the controller.
    #[doc(hidden)]
    pub fn replace_settings_for_test(&self, settings: Settings) {
        let mut guard = match self.settings.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *guard = settings;
    }

    /// Clone the live settings under a short read lock.  Callers that
    /// need more than one field should call this once — repeatedly
    /// re-acquiring the read guard is cheap but noisier.  Poisoned
    /// locks are recovered via `into_inner` because a writer that
    /// panicked mid-swap still leaves a valid `Settings` behind.
    /// `pub` for the test that asserts hot-reload propagates fresh
    /// settings into the snapshot path.
    #[doc(hidden)]
    pub fn settings_snapshot(&self) -> Settings {
        match self.settings.read() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// Core of `BrowserArtefactSink::publish_file_as_artefact` — extracted
    /// so tests can exercise the put-in-store side without touching the
    /// trait-object bus.  Returns the minted `(file_id, artefact_id)` on
    /// success, `None` when the file couldn't be read.
    pub(crate) fn publish_file_as_artefact_impl(
        &self,
        chat_id: &str,
        path: &std::path::Path,
    ) -> Option<(String, String)> {
        // Match the 25 MB cap used by `SseOutput::send_file` — keeps a
        // runaway tool from blowing memory regardless of which controller
        // is the source of the file.
        const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
        match std::fs::metadata(path) {
            Ok(m) if m.len() > MAX_FILE_BYTES => {
                tracing::warn!(
                    path = %path.display(),
                    bytes = m.len(),
                    "file too large to publish as browser artefact",
                );
                return None;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "stat failed — cannot publish browser artefact",
                );
                return None;
            }
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "read failed — cannot publish browser artefact",
                );
                return None;
            }
        };

        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        let mime = super::responses::mime_for_extension(path);
        let inline_image = mime.starts_with("image/");
        let bytes_len = bytes.len();

        let file_id = format!(
            "f{}",
            self.file_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let file_url = format!("/api/files/{file_id}");
        let file_entry = FileEntry {
            bytes,
            mime: mime.clone(),
            name: name.clone(),
        };
        if let Some(dir) = self.data_dir.as_ref() {
            FileStore::persist_static(dir, &file_id, &file_entry);
        }
        // Recover from poisoning so a previous panicked holder doesn't
        // permanently stop the cache from accepting new entries (the
        // disk write above already happened, so a silent skip on Err
        // would orphan the bytes for in-process readers until restart).
        let mut s = match self.files.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        s.put(file_id.clone(), file_entry);
        drop(s);

        let kind = if inline_image {
            crate::message::ArtefactKind::Image
        } else {
            crate::message::ArtefactKind::Other
        };
        let artefact_id = format!(
            "a{}",
            self.artefact_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let artefact_url = format!("/#/artefacts/{artefact_id}");
        let metadata = serde_json::json!({
            "file_url": file_url,
            "file_name": name,
            "bytes": bytes_len,
        });
        let entry = ArtefactEntry {
            chat_id: chat_id.to_string(),
            kind,
            title: name.clone(),
            // `content` holds the served URL rather than raw bytes —
            // matches `SseOutput::send_file`'s inline-image artefact
            // branch so the reader's image fallback kicks in for
            // anything with an `image/*` mime.
            content: file_url.clone(),
            mime_type: mime.clone(),
            metadata: Some(metadata.clone()),
            tool_use_id: None,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        if let Some(dir) = self.data_dir.as_ref() {
            ArtefactStore::persist_static(dir, &artefact_id, &entry);
        }
        let mut s = match self.artefacts.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        s.put(artefact_id.clone(), entry);
        drop(s);

        // Best-effort live SSE broadcast: if a browser is currently
        // subscribed to this chat, it sees the new file + artefact
        // without needing to re-list.  `try_lock` keeps us sync-safe
        // — a busy chats map just means the browser picks it up on
        // the next poll / reload.
        if let Ok(guard) = self.chats.try_lock()
            && let Some(handle) = guard.get(chat_id).cloned()
        {
            let _ = handle.events.send(SseEvent::File {
                name: name.clone(),
                mime_type: mime.clone(),
                url: file_url,
                inline_image,
            });
            let _ = handle.events.send(SseEvent::Artefact {
                id: artefact_id.clone(),
                kind,
                title: name,
                url: artefact_url,
                bytes: bytes_len,
                metadata: Some(metadata),
            });
        }

        Some((file_id, artefact_id))
    }
}

impl super::super::BrowserArtefactSink for HttpState {
    fn publish_file_as_artefact(&self, chat_id: &str, path: &std::path::Path) {
        let _ = self.publish_file_as_artefact_impl(chat_id, path);
    }
}
