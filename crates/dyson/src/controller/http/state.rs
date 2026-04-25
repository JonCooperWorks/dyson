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
//
// ─── Lock topology ─────────────────────────────────────────────────────────
//
// | Field                        | Lock kind          | Acquired from        | Held while                                     |
// |------------------------------|--------------------|----------------------|------------------------------------------------|
// | `settings`                   | `std::RwLock`      | reads + hot-reload   | a short read or a swap                         |
// | `chats`                      | `tokio::Mutex`     | every API request    | a single map insert/get/clone                  |
// | `order`                      | `tokio::Mutex`     | list / mint / delete | one Vec mutation                               |
// | `files`                      | `std::Mutex`       | sync `Output::send_file` | HashMap put/get + Vec push                  |
// | `artefacts`                  | `std::Mutex`       | sync `Output::send_artefact` + list/get | HashMap put/get + Vec push        |
// | `runtime_model`              | `std::Mutex`       | `post_model`, `turns`| one `Option<(String, String)>` swap or clone   |
// | `sse_tickets`                | `std::Mutex`       | mint + consume       | HashMap insert/remove                          |
// | `titles`                     | `std::Mutex`       | conversations list   | HashMap insert/get                             |
// | `ChatHandle.agent`           | `tokio::Mutex`     | one turn at a time   | the entire turn (`Agent::run` is `&mut self`)  |
// | `ChatHandle.cancel`          | `tokio::Mutex`     | turn start + cancel  | one `Option<CancellationToken>` swap           |
// | `ChatHandle.replay`          | `std::Mutex`       | every emit + every reconnect | one `VecDeque` push or scan            |
//
// All `std::Mutex` callers recover from poisoning via `into_inner()`
// — a previous panic-while-holding-lock leaves the wrapped value
// well-formed; silently skipping on `Err` would permanently disable
// the cache for the rest of the process.
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
    /// Broadcast channel for SSE subscribers.  Capacity 4096 — a slow
    /// subscriber that lags by more than that will see "lag" gaps in
    /// the live stream, but the rolling replay buffer below is the
    /// authoritative recovery path so nothing is permanently lost as
    /// long as the buffer cap covers the disconnect window.
    pub(crate) events: broadcast::Sender<SseEvent>,
    /// Rolling buffer of the most recent `RING_CAP` events tagged
    /// with monotonic ids.  A reconnecting EventSource passes
    /// `Last-Event-ID: <n>` and we replay everything with id > n
    /// before attaching the live broadcast subscriber.  Behind a
    /// `std::sync::Mutex` because every emit takes the lock and the
    /// critical section is a single VecDeque push.  Wrapped in
    /// `Arc` so per-turn `SseOutput` can clone a handle and push
    /// without re-locking the chats map.
    pub(crate) replay: Arc<std::sync::Mutex<EventRing>>,
    /// Cancellation token shared with the running turn (if any).
    pub(crate) cancel: Mutex<Option<CancellationToken>>,
    /// `true` while a turn is in flight.
    pub(crate) busy: std::sync::atomic::AtomicBool,
}

/// Per-chat replay ring buffer.  Capacity is small and fixed so the
/// memory bound is obvious — covers a normal reconnect window.
pub(crate) struct EventRing {
    pub(crate) entries: std::collections::VecDeque<(u64, SseEvent)>,
    /// Monotonic counter; the next event minted gets `next_id`.
    pub(crate) next_id: u64,
}

impl EventRing {
    /// Maximum entries kept.  256 covers a normal reconnect window
    /// (browser refresh, brief network blip) without giving a
    /// runaway producer an unbounded buffer.
    const CAP: usize = 256;

    pub(crate) fn new() -> Self {
        Self {
            entries: std::collections::VecDeque::with_capacity(Self::CAP),
            next_id: 1,
        }
    }

    /// Push an event onto the ring and return the id it was tagged
    /// with.  Wraps oldest-first when at capacity.
    pub(crate) fn push(&mut self, evt: SseEvent) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        if self.entries.len() >= Self::CAP {
            self.entries.pop_front();
        }
        self.entries.push_back((id, evt));
        id
    }

    /// Snapshot of every entry with id > `since`.  Used by SSE
    /// reconnect to replay only what the client missed.  Cloned
    /// because the caller holds the mutex briefly and then iterates.
    pub(crate) fn since(&self, since: u64) -> Vec<(u64, SseEvent)> {
        self.entries
            .iter()
            .filter(|(id, _)| *id > since)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod ring_tests {
    use super::*;
    use super::SseEvent;
    // Stand-alone tests for the rolling buffer — no async, no
    // tokio.  Asserts the FIFO bound and the `since` cutoff that
    // the SSE replay path depends on.
    #[test]
    fn event_ring_evicts_oldest_at_cap() {
        let mut r = EventRing::new();
        let cap = EventRing::CAP;
        for n in 0..(cap + 5) {
            r.push(SseEvent::Text { delta: format!("e{n}") });
        }
        assert_eq!(r.entries.len(), cap, "ring must hold the cap");
        // Earliest entry id is 6 (5 evicted + 1-based).
        assert_eq!(r.entries.front().map(|(i, _)| *i), Some(6));
        assert_eq!(r.entries.back().map(|(i, _)| *i), Some((cap + 5) as u64));
    }

    #[test]
    fn event_ring_since_skips_seen_events() {
        let mut r = EventRing::new();
        for n in 0..5 {
            r.push(SseEvent::Text { delta: format!("e{n}") });
        }
        let after = r.since(2);
        let ids: Vec<u64> = after.iter().map(|(i, _)| *i).collect();
        assert_eq!(ids, vec![3, 4, 5]);
        assert!(r.since(99).is_empty(), "since past tip yields nothing");
    }
}

impl ChatHandle {
    pub(crate) fn new(title: String) -> Self {
        let (tx, _) = broadcast::channel(4096);
        Self {
            title,
            agent: Mutex::new(None),
            events: tx,
            replay: Arc::new(std::sync::Mutex::new(EventRing::new())),
            cancel: Mutex::new(None),
            busy: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Emit an event: push into the rolling replay buffer (so a
    /// reconnect can replay it) AND broadcast to live subscribers
    /// (so any currently-connected EventSource sees it now).  The
    /// returned id is the monotonic event id used by SSE clients
    /// for `Last-Event-ID` resumption.
    pub(crate) fn emit(&self, evt: SseEvent) -> u64 {
        let id = {
            let mut ring = match self.replay.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            ring.push(evt.clone())
        };
        let _ = self.events.send(evt);
        id
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
    /// Enforce a Host-header allowlist of `{127.0.0.1, ::1, localhost}`
    /// for `/api/*` requests.  Set `true` only when the bind is
    /// loopback AND auth is `DangerousNoAuth` — that combination is
    /// vulnerable to DNS rebinding because a browser running on
    /// `attacker.example.com` (resolved to 127.0.0.1) can otherwise
    /// fire origin-bypassing requests at the API.  Reverse-proxy /
    /// bearer / OIDC deployments stay off the gate so the public Host
    /// the proxy presents (e.g. `dyson.example.com`) doesn't 421.
    pub(crate) loopback_only_host_check: bool,
    /// Single-user lock: if set, only callers whose authenticated
    /// identity matches are allowed to consume an SSE ticket (and,
    /// going forward, any other identity-gated surface).  Sourced
    /// from the OIDC `allowed_sub` config — the regular auth path
    /// already enforces it on `validate_request`, this is the
    /// counterpart for ticket-based requests that bypass that path.
    /// Wrapped in a `Mutex` so test hooks can swap the value into
    /// place after construction; production sets it once at start
    /// and never mutates.
    pub(crate) allowed_identity: std::sync::Mutex<Option<String>>,
    /// Cache of `(chat_id → first-user-text title)` so the
    /// `/api/conversations` list endpoint isn't `O(n)` history loads
    /// per call.  Populated on first hydration miss; invalidated on
    /// turn save when the first user message of a chat would have
    /// changed.  Not on disk: rebuilds on cold start with one load
    /// per chat.
    pub(crate) titles: std::sync::Mutex<HashMap<String, String>>,
    /// One-shot SSE tickets.  EventSource can't send headers, so the
    /// SPA exchanges its bearer for a single-use, short-lived ticket
    /// (`POST /api/auth/sse-ticket`) and passes it as
    /// `?access_token=<ticket>` on the SSE connect.  The dispatcher
    /// looks the ticket up here, removes it (single-use), and trusts
    /// the bound identity for that one request.  Replaces the older
    /// raw-bearer-in-URL path so a leaked log line carrying
    /// `?access_token=` discloses at most a 30s, used-once token.
    pub(crate) sse_tickets: std::sync::Mutex<HashMap<String, SseTicket>>,
}

/// Stored per-ticket: the bound identity (so we can attach it on
/// validation) and the wall-clock expiry.  TTL is 30s — long enough
/// to round-trip a fetch + open an EventSource even on slow links,
/// short enough that a leak is bounded.
#[derive(Clone)]
pub(crate) struct SseTicket {
    pub(crate) identity: String,
    pub(crate) expires_at: std::time::Instant,
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
        loopback_only_host_check: bool,
        allowed_identity: Option<String>,
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
            loopback_only_host_check,
            sse_tickets: std::sync::Mutex::new(HashMap::new()),
            titles: std::sync::Mutex::new(HashMap::new()),
            allowed_identity: std::sync::Mutex::new(allowed_identity),
        }
    }

    /// Mint a one-shot SSE ticket bound to `identity`.  Returns the
    /// opaque token string the caller hands to the SPA.  The ticket
    /// expires after 30 seconds; expired entries are pruned lazily
    /// here so the map can't grow without bound when the SPA fetches
    /// tickets it never uses.
    pub(crate) fn mint_sse_ticket(&self, identity: &str) -> String {
        // 24 random bytes → 32 base64 chars.  Plenty of entropy and
        // shorter than a UUID for URL ergonomics.
        use rand::RngExt;
        let mut buf = [0u8; 24];
        rand::rng().fill(&mut buf);
        use base64::Engine;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf);
        let now = std::time::Instant::now();
        let entry = SseTicket {
            identity: identity.to_string(),
            expires_at: now + std::time::Duration::from_secs(30),
        };
        let mut guard = match self.sse_tickets.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        // Lazy prune — drop everything that's already expired.
        guard.retain(|_, e| e.expires_at > now);
        guard.insert(token.clone(), entry);
        token
    }

    /// Single-use ticket consume: returns the bound identity if the
    /// token is present, unexpired, and (when the controller is
    /// locked via `allowed_identity`) the bound identity matches
    /// the lock.  Always removes the entry — even on expiry or
    /// identity-mismatch — so a single-use token can't be retried.
    pub(crate) fn consume_sse_ticket(&self, token: &str) -> Option<String> {
        let mut guard = match self.sse_tickets.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let entry = guard.remove(token)?;
        if entry.expires_at <= std::time::Instant::now() {
            return None;
        }
        let allowed = match self.allowed_identity.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        };
        if let Some(allowed) = allowed
            && entry.identity != allowed
        {
            tracing::warn!(
                ticket_identity = %entry.identity,
                allowed,
                "SSE ticket bound to non-allowed identity rejected",
            );
            return None;
        }
        Some(entry.identity)
    }

    /// Mint a fresh chat id that doesn't collide with any existing chat
    /// (in-memory, rotated archive, or referenced by an artefact).
    /// `next_id` is primed at startup from the max `c-NNNN` ever seen
    /// on disk so freshly-minted ids never reuse a slot that still has
    /// artefact metadata tagged to it.
    pub(crate) async fn mint_id(&self) -> String {
        let mut tries: u32 = 0;
        loop {
            let n = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let id = format!("c-{n:04}");
            let chats = self.chats.lock().await;
            if chats.contains_key(&id) {
                tries += 1;
                // The startup scan should prime `next_id` past every
                // known chat, so collisions here are rare.  A
                // sustained streak suggests the scan missed a slot or
                // the counter wrapped — surface it once so it shows
                // up in logs without spamming on every iteration.
                if tries == 16 {
                    tracing::warn!(
                        tries,
                        next_id = n,
                        "mint_id: 16+ consecutive collisions — startup id scan may be incomplete"
                    );
                }
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
        // the next poll / reload.  Log when we miss the lock so a
        // sustained pattern of misses (live contention with the
        // /api/conversations list path) is observable.
        let chats_guard = match self.chats.try_lock() {
            Ok(g) => Some(g),
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    chat_id,
                    "publish_file_as_artefact: chats try_lock missed; \
                     event will arrive on next browser poll"
                );
                None
            }
        };
        if let Some(guard) = chats_guard
            && let Some(handle) = guard.get(chat_id).cloned()
        {
            handle.emit(SseEvent::File {
                name: name.clone(),
                mime_type: mime.clone(),
                url: file_url,
                inline_image,
                parent_tool_id: None,
            });
            handle.emit(SseEvent::Artefact {
                id: artefact_id.clone(),
                kind,
                title: name,
                url: artefact_url,
                bytes: bytes_len,
                metadata: Some(metadata),
                parent_tool_id: None,
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
