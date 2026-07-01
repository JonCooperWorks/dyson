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
// | `runtime_model`              | `std::Mutex`       | `post_model`, `turns`| one typed provider/model selection swap/clone  |
// | `sse_tickets`                | `std::Mutex`       | mint + consume       | HashMap insert/remove                          |
// | `titles`                     | `std::Mutex`       | conversations list   | HashMap insert/get                             |
// | `quiesced`                   | `AtomicBool`       | admin + turns        | zero-lock snapshot/turn admission latch        |
// | `ChatHandle.agent`           | `tokio::Mutex`     | one turn at a time   | the entire turn (`Agent::run` is `&mut self`)  |
// | `ChatHandle.reloader`        | `tokio::Mutex`     | one turn at a time   | a `check()` + optional rebuild                 |
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
use crate::config::{ActiveProvider, Settings};
use crate::feedback::FeedbackStore;
use crate::message::Message;
use crate::util::resolve_tilde;

use super::ClientRegistry;
use super::stores::{ArtefactEntry, ArtefactStore, FileEntry, FileStore, max_file_id};
use super::wire::{AuthMode, SseEvent};

/// One pending POST /turn that arrived while a turn was already running.
///
/// Stored verbatim (the base64 attachment payload is what the wire
/// already carries) so persistence is trivial and there's no decode/
/// re-encode round-trip on the queue path.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct QueuedTurn {
    pub(crate) prompt: String,
    #[serde(default)]
    pub(crate) attachments: Vec<QueuedAttachment>,
    #[serde(default)]
    pub(crate) provider: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) queue_mode: Option<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct QueuedAttachment {
    pub(crate) mime_type: String,
    #[serde(default)]
    pub(crate) name: Option<String>,
    pub(crate) data_base64: String,
}

/// Runtime model switch selected through the HTTP controller.
///
/// This intentionally is not a tuple: every consumer must go through the
/// same validation and settings-application path, so an unknown provider
/// cannot silently fall back to the registry default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeModelSelection {
    provider: String,
    model: String,
}

impl RuntimeModelSelection {
    pub(crate) fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, String> {
        let provider = provider.into().trim().to_string();
        let model = model.into().trim().to_string();
        if provider.is_empty() {
            return Err("provider must not be empty".to_string());
        }
        if model.is_empty() {
            return Err("model must not be empty".to_string());
        }
        Ok(Self { provider, model })
    }

    pub(crate) fn provider(&self) -> &str {
        &self.provider
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn apply_to_settings(&self, settings: &mut Settings) -> Result<(), String> {
        let pc = settings
            .providers
            .get(&self.provider)
            .ok_or_else(|| format!("unknown provider '{}'", self.provider))?;
        settings.agent.provider = pc.provider_type.clone();
        settings.agent.api_key = pc.api_key.clone();
        settings.agent.base_url = pc.base_url.clone();
        settings.agent.model = self.model.clone();
        settings.active_provider = ActiveProvider::new(self.provider.clone(), self.model.clone());
        Ok(())
    }
}

/// Outcome of an enqueue attempt.
pub(crate) enum EnqueueResult {
    Queued { position: usize },
    Full,
}

/// Maximum queued POSTs per chat before falling back to 409.  At
/// MAX_TURN_BODY=25 MiB per turn this caps a single chat's queue at
/// ~400 MiB resident worst case, which is fine for the deployment
/// scale.  When full the controller still rejects with 409 so the
/// SPA can surface backpressure to the user.
pub(crate) const QUEUE_CAP: usize = 16;

/// Per-chat handle.  Agent built lazily on first turn so that listing chats
/// or creating an empty one is cheap.
pub(crate) struct ChatHandle {
    title: std::sync::RwLock<String>,
    /// The chat's id.  Carried on the handle so persistence helpers
    /// (queue, future state) don't need it threaded from the call site.
    pub(crate) chat_id: String,
    /// Resolved on-disk path for the persisted queue, or `None` when
    /// `data_dir` is unset (memory-only deployment).
    pub(crate) queue_path: Option<PathBuf>,
    /// Pending turns accumulated while `busy=true`.  Drained at the end
    /// of the in-flight turn and coalesced into one `agent.run()` call.
    /// `std::Mutex` because the lock is only held for VecDeque ops.
    pub(crate) queued: std::sync::Mutex<std::collections::VecDeque<QueuedTurn>>,
    /// Agent — `None` until first turn, then populated.  Behind tokio Mutex
    /// because `Agent::run` requires `&mut self` and turns are serialised
    /// per chat.
    pub(crate) agent: Mutex<Option<Agent>>,
    /// Workspace mtime watcher — built alongside the agent on first turn.
    /// Before each turn we call `check()` and rebuild the cached agent
    /// when the workspace changed on disk.  Without this, a skill written
    /// by `SelfImprovementDream` or a manual edit to `MEMORY.md` /
    /// `SOUL.md` is invisible until the chat is deleted or the process
    /// restarts — the terminal controller has the same wiring via
    /// `check_and_reload_agent`, but HTTP caches one agent per chat for
    /// the lifetime of the chat so the rebuild has to fire here instead.
    /// Config (`dyson.json`) reloads are already handled by
    /// `subscribe_settings_updates` updating `state.settings`, so this
    /// reloader only watches the workspace directory.
    pub(crate) reloader: Mutex<Option<crate::config::hot_reload::HotReloader>>,
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
    /// Instant of the last turn activity on this chat.  Drives idle
    /// eviction of the cached `agent` (see
    /// [`HttpState::evict_idle_agents`]) — without it, every chat ever
    /// opened kept its full in-memory history (including restored
    /// base64 images) resident until deletion or restart.
    pub(crate) last_used: std::sync::Mutex<std::time::Instant>,
}

/// Per-chat replay ring buffer.  Capacity is small and fixed so the
/// memory bound is obvious — covers a normal reconnect window.
pub(crate) struct EventRing {
    pub(crate) entries: std::collections::VecDeque<(u64, SseEvent)>,
    /// Monotonic counter; the next event minted gets `next_id`.
    pub(crate) next_id: u64,
}

impl EventRing {
    /// Maximum entries kept.  Matched to the broadcast channel
    /// capacity (`broadcast::channel(4096)` in `ChatHandle::new`) so
    /// a subscriber that hits `RecvError::Lagged` can still resume
    /// from the ring — with the previous 256-entry ring, events
    /// 257..4096 behind the cursor were dropped from the channel
    /// AND already evicted from the ring, leaving a permanent gap
    /// for that one client.  `SseEvent` payloads are small (text
    /// deltas, tool refs); 4096 of them is a few MB per active chat
    /// at worst, fine for the deployment scale.
    const CAP: usize = 4096;

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

    /// Drop every entry but keep the monotonic counter.  Called at
    /// end-of-turn so a fresh `EventSource` opening for the *next*
    /// turn (no `Last-Event-ID`, server treats `since=0`) doesn't
    /// replay the previous turn's text deltas into the new agent
    /// placeholder — the visible "subsequent message responds with
    /// the last message" duplication bug.  Keeping `next_id`
    /// monotonic protects any live subscriber from observing a
    /// regressed id on the next emit.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod ring_tests {
    use super::SseEvent;
    use super::*;
    // Stand-alone tests for the rolling buffer — no async, no
    // tokio.  Asserts the FIFO bound and the `since` cutoff that
    // the SSE replay path depends on.
    #[test]
    fn event_ring_evicts_oldest_at_cap() {
        let mut r = EventRing::new();
        let cap = EventRing::CAP;
        for n in 0..(cap + 5) {
            r.push(SseEvent::Text {
                delta: format!("e{n}"),
            });
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
            r.push(SseEvent::Text {
                delta: format!("e{n}"),
            });
        }
        let after = r.since(2);
        let ids: Vec<u64> = after.iter().map(|(i, _)| *i).collect();
        assert_eq!(ids, vec![3, 4, 5]);
        assert!(r.since(99).is_empty(), "since past tip yields nothing");
    }

    #[test]
    fn event_ring_clear_drops_entries_but_keeps_id_monotonic() {
        // The ring is per-turn state.  At end-of-turn we wipe the
        // entries so a fresh EventSource opening for the *next* turn
        // (which sends no Last-Event-ID) doesn't replay the previous
        // turn's text deltas into the new agent placeholder — that's
        // the visible "subsequent message responds with the last
        // message" duplication bug.
        let mut r = EventRing::new();
        r.push(SseEvent::Text {
            delta: "stale-a".into(),
        });
        let last_done = r.push(SseEvent::Done);
        assert_eq!(r.since(0).len(), 2, "preconditions");

        r.clear();
        assert!(r.entries.is_empty(), "clear must wipe entries");
        assert!(
            r.since(0).is_empty(),
            "since(0) returns nothing after clear"
        );

        // next_id stays monotonic so any live subscriber that's still
        // around (e.g. the previous turn's ES racing to receive Done)
        // doesn't observe a regressed id from a future emit.
        let next = r.push(SseEvent::Text {
            delta: "fresh".into(),
        });
        assert!(
            next > last_done,
            "next_id must remain monotonic across clear ({next} <= {last_done})"
        );
    }

    #[tokio::test]
    async fn mid_turn_text_admission_persists_empty_queue_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let handle = ChatHandle::new("c-test".to_string(), "test".to_string(), Some(tmp.path()));
        handle
            .enqueue_turn(QueuedTurn {
                prompt: "while you work".to_string(),
                attachments: Vec::new(),
                provider: None,
                model: None,
                queue_mode: Some("next_tool_call".to_string()),
            })
            .await;
        let qpath = handle.queue_path.clone().unwrap();
        assert!(qpath.exists(), "enqueue persists queue file");

        let mut admitted = Vec::new();
        let count = handle
            .admit_text_only_queued_turns(|message| {
                admitted.push(message);
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(count, 1);
        assert_eq!(admitted.len(), 1);
        assert!(
            !qpath.exists(),
            "admitting the last queued turn removes queue.json"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_tool_call_admission_consumes_before_callback_so_it_cannot_repeat() {
        let tmp = tempfile::tempdir().unwrap();
        let handle = Arc::new(ChatHandle::new(
            "c-test".to_string(),
            "test".to_string(),
            Some(tmp.path()),
        ));
        handle
            .enqueue_turn(QueuedTurn {
                prompt: "only once".to_string(),
                attachments: Vec::new(),
                provider: None,
                model: None,
                queue_mode: Some("next_tool_call".to_string()),
            })
            .await;

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let release_gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

        let first_handle = Arc::clone(&handle);
        let first_release_gate = Arc::clone(&release_gate);
        let first = tokio::spawn(async move {
            let mut admitted = Vec::new();
            let count = first_handle
                .admit_text_only_queued_turns(|message| {
                    admitted.push(message);
                    started_tx.send(()).unwrap();
                    let (lock, cvar) = &*first_release_gate;
                    let released = lock.lock().unwrap();
                    let (_released, timeout) = cvar
                        .wait_timeout_while(
                            released,
                            std::time::Duration::from_secs(2),
                            |released| !*released,
                        )
                        .unwrap();
                    assert!(
                        !timeout.timed_out(),
                        "test timed out waiting to release first admit"
                    );
                    Ok(())
                })
                .await
                .unwrap();
            (count, admitted.len())
        });

        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();

        let mut second_admitted = Vec::new();
        let second_count = handle
            .admit_text_only_queued_turns(|message| {
                second_admitted.push(message);
                Ok(())
            })
            .await
            .unwrap();

        let (lock, cvar) = &*release_gate;
        *lock.lock().unwrap() = true;
        cvar.notify_one();
        let (first_count, first_admitted) = first.await.unwrap();

        assert_eq!(first_count, 1);
        assert_eq!(first_admitted, 1);
        assert_eq!(
            second_count, 0,
            "a next-tool queued turn must be invisible to later admission callbacks once admitted"
        );
        assert!(second_admitted.is_empty());
        assert_eq!(handle.queued.lock().unwrap().len(), 0);
        assert!(!handle.queue_path.clone().unwrap().exists());
    }

    #[tokio::test]
    async fn multiple_next_tool_call_turns_admit_fifo_and_only_once() {
        let tmp = tempfile::tempdir().unwrap();
        let handle = ChatHandle::new("c-test".to_string(), "test".to_string(), Some(tmp.path()));
        for prompt in ["first", "second"] {
            handle
                .enqueue_turn(QueuedTurn {
                    prompt: prompt.to_string(),
                    attachments: Vec::new(),
                    provider: None,
                    model: None,
                    queue_mode: Some("next_tool_call".to_string()),
                })
                .await;
        }

        let mut admitted = Vec::new();
        let count = handle
            .admit_text_only_queued_turns(|message| {
                if let Some(crate::message::ContentBlock::Text { text }) = message.content.first() {
                    admitted.push(text.clone());
                }
                Ok(())
            })
            .await
            .unwrap();

        let mut repeated = Vec::new();
        let repeated_count = handle
            .admit_text_only_queued_turns(|message| {
                repeated.push(message);
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(count, 2);
        assert_eq!(admitted, vec!["first", "second"]);
        assert_eq!(repeated_count, 0);
        assert!(repeated.is_empty());
        assert_eq!(handle.queued.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn normal_text_queued_turns_keep_legacy_mid_turn_admission() {
        let tmp = tempfile::tempdir().unwrap();
        let handle = ChatHandle::new("c-test".to_string(), "test".to_string(), Some(tmp.path()));
        handle
            .enqueue_turn(QueuedTurn {
                prompt: "later".to_string(),
                attachments: Vec::new(),
                provider: None,
                model: None,
                queue_mode: Some("normal".to_string()),
            })
            .await;

        let mut admitted = Vec::new();
        let count = handle
            .admit_text_only_queued_turns(|message| {
                admitted.push(message);
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(count, 1);
        assert_eq!(admitted.len(), 1);
        assert_eq!(handle.queued.lock().unwrap().len(), 0);
        assert!(!handle.queue_path.clone().unwrap().exists());
    }

    #[tokio::test]
    async fn mid_turn_admission_leaves_attachment_turns_for_end_of_turn_drain() {
        let tmp = tempfile::tempdir().unwrap();
        let handle = ChatHandle::new("c-test".to_string(), "test".to_string(), Some(tmp.path()));
        handle
            .enqueue_turn(QueuedTurn {
                prompt: "see attached".to_string(),
                attachments: vec![QueuedAttachment {
                    mime_type: "text/plain".to_string(),
                    name: Some("note.txt".to_string()),
                    data_base64: "aGVsbG8=".to_string(),
                }],
                provider: None,
                model: None,
                queue_mode: Some("next_tool_call".to_string()),
            })
            .await;

        let count = handle
            .admit_text_only_queued_turns(|_| panic!("attachment turn must not admit mid-turn"))
            .await
            .unwrap();

        assert_eq!(count, 0);
        assert_eq!(handle.queued.lock().unwrap().len(), 1);
        assert!(handle.queue_path.clone().unwrap().exists());
    }
}

impl ChatHandle {
    /// Create an empty handle — does not touch disk.
    ///
    /// Call sites that have a `data_dir` should follow up with
    /// `hydrate_queue_from_disk()` so any queued turns persisted by a
    /// previous process are picked up.
    pub(crate) fn new(chat_id: String, title: String, data_dir: Option<&std::path::Path>) -> Self {
        let (tx, _) = broadcast::channel(4096);
        let queue_path = data_dir.map(|d| d.join(&chat_id).join("queue.json"));
        Self {
            title: std::sync::RwLock::new(title),
            chat_id,
            queue_path,
            queued: std::sync::Mutex::new(std::collections::VecDeque::new()),
            agent: Mutex::new(None),
            reloader: Mutex::new(None),
            events: tx,
            replay: Arc::new(std::sync::Mutex::new(EventRing::new())),
            cancel: Mutex::new(None),
            busy: std::sync::atomic::AtomicBool::new(false),
            last_used: std::sync::Mutex::new(std::time::Instant::now()),
        }
    }

    /// Record turn activity for idle-eviction accounting.
    pub(crate) fn touch(&self) {
        let mut guard = self
            .last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = std::time::Instant::now();
    }

    /// How long since the last recorded turn activity.
    pub(crate) fn idle_for(&self) -> std::time::Duration {
        self.last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
    }

    pub(crate) fn title(&self) -> String {
        match self.title.read() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    pub(crate) fn set_title(&self, title: String) {
        match self.title.write() {
            Ok(mut g) => *g = title,
            Err(p) => *p.into_inner() = title,
        }
    }

    /// Read the persisted queue from disk and replace the in-memory
    /// VecDeque.  Idempotent and safe to call before any push/pop —
    /// gracefully handles a missing or malformed file by leaving the
    /// queue empty.
    pub(crate) async fn hydrate_queue_from_disk(&self) {
        let Some(path) = &self.queue_path else { return };
        let bytes = match tokio::fs::read(path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!(error = %e, chat_id = %self.chat_id, "failed to read queued turns from disk");
                return;
            }
        };
        match serde_json::from_slice::<Vec<QueuedTurn>>(&bytes) {
            Ok(items) => {
                let mut q = self.queued.lock().unwrap_or_else(|p| p.into_inner());
                *q = items.into();
            }
            Err(e) => {
                tracing::warn!(error = %e, chat_id = %self.chat_id, "queued turns file malformed; ignoring")
            }
        }
    }

    /// Push one turn into the queue and persist.  Returns the
    /// 1-indexed position (`Queued{1}` is the next to drain) or
    /// `Full` when the cap is hit.
    pub(crate) async fn enqueue_turn(&self, turn: QueuedTurn) -> EnqueueResult {
        let position = {
            let mut q = self.queued.lock().unwrap_or_else(|p| p.into_inner());
            if q.len() >= QUEUE_CAP {
                return EnqueueResult::Full;
            }
            q.push_back(turn);
            q.len()
        };
        self.persist_queue().await;
        EnqueueResult::Queued { position }
    }

    /// Drain every queued turn and persist (deletes the file when
    /// empty).  Returns the drained turns in FIFO order.
    pub(crate) async fn drain_queued_turns(&self) -> Vec<QueuedTurn> {
        let drained: Vec<_> = {
            let mut q = self.queued.lock().unwrap_or_else(|p| p.into_inner());
            q.drain(..).collect()
        };
        if !drained.is_empty() {
            self.persist_queue().await;
        }
        drained
    }

    /// Admit queued text-only turns while a tool-using agent turn is
    /// still running.  This preserves the pre-existing default queue
    /// behavior: plain text queued during a run is visible to the next
    /// LLM iteration after the current tool batch.  Attachment turns
    /// remain queued for the existing end-of-turn path because resolving
    /// media belongs to `Agent::run_with_attachments`, not this
    /// controller-side queue.
    pub(crate) async fn admit_text_only_queued_turns<F>(
        &self,
        mut admit: F,
    ) -> crate::error::Result<usize>
    where
        F: FnMut(Message) -> crate::error::Result<()>,
    {
        let candidates = {
            let mut q = self.queued.lock().unwrap_or_else(|p| p.into_inner());
            let mut candidates = Vec::new();
            while q.front().is_some_and(|turn| {
                turn.attachments.is_empty() && Self::admits_mid_turn(turn.queue_mode.as_deref())
            }) {
                if let Some(turn) = q.pop_front() {
                    candidates.push(turn);
                }
            }
            candidates
        };
        if candidates.is_empty() {
            return Ok(0);
        }
        self.persist_queue().await;

        let mut admitted = 0usize;
        for turn in &candidates {
            if let Err(err) = admit(Message::user(&turn.prompt)) {
                if admitted < candidates.len() {
                    self.requeue_front(candidates[admitted..].to_vec()).await;
                }
                return Err(err);
            }
            admitted += 1;
        }
        Ok(admitted)
    }

    async fn requeue_front(&self, mut turns: Vec<QueuedTurn>) {
        if turns.is_empty() {
            return;
        }
        {
            let mut q = self.queued.lock().unwrap_or_else(|p| p.into_inner());
            while let Some(turn) = turns.pop() {
                q.push_front(turn);
            }
        }
        self.persist_queue().await;
    }

    fn admits_mid_turn(queue_mode: Option<&str>) -> bool {
        matches!(queue_mode, None | Some("normal") | Some("next_tool_call"))
    }

    /// Drop every queued turn without acting on them.  Used by `/clear`
    /// so a wiped chat doesn't resurrect prompts the user typed during
    /// the previous run.
    pub(crate) async fn clear_queued(&self) {
        let had_any = {
            let mut q = self.queued.lock().unwrap_or_else(|p| p.into_inner());
            let any = !q.is_empty();
            q.clear();
            any
        };
        if had_any {
            self.persist_queue().await;
        }
    }

    /// Snapshot the queue and write it to disk.  Deletes the file when
    /// the queue is empty so an empty queue + restart is observably
    /// the same as a fresh chat.
    async fn persist_queue(&self) {
        let Some(path) = &self.queue_path else { return };
        let snapshot: Vec<QueuedTurn> = {
            let q = self.queued.lock().unwrap_or_else(|p| p.into_inner());
            q.iter().cloned().collect()
        };
        if snapshot.is_empty() {
            if let Err(e) = tokio::fs::remove_file(path).await
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(error = %e, chat_id = %self.chat_id, "failed to remove empty queue file");
            }
            return;
        }
        if let Some(parent) = path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            tracing::warn!(error = %e, chat_id = %self.chat_id, "failed to create queue dir");
            return;
        }
        match serde_json::to_vec(&snapshot) {
            Ok(json) => {
                if let Err(e) = tokio::fs::write(path, json).await {
                    tracing::warn!(error = %e, chat_id = %self.chat_id, "failed to persist queued turns");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, chat_id = %self.chat_id, "failed to serialize queued turns")
            }
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

    /// Drop every entry from the replay ring.  Called by the turn
    /// dispatcher right after `Done` so a fresh `EventSource` opened
    /// for the next turn doesn't see the previous turn's events.
    /// See `EventRing::clear` for the bug this prevents.
    pub(crate) fn reset_replay(&self) {
        let mut ring = match self.replay.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        ring.clear();
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
    /// In-memory override for the selected provider/model applied to any
    /// agent built after `post_model` has run — `state.settings` is a
    /// frozen snapshot from startup, so without this override a new
    /// conversation (and any first-use agent build) would reuse the
    /// startup model.  Cleared on process restart — the persisted
    /// `dyson.json` write is what carries the choice across restarts.
    pub(crate) runtime_model: std::sync::Mutex<Option<RuntimeModelSelection>>,
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
    /// (`POST /api/auth/sse-ticket`) which the controller sets as an
    /// `HttpOnly; SameSite=Strict; Path=/api/conversations` cookie
    /// (`dyson_sse=<ticket>`).  Same-origin EventSource sends the
    /// cookie automatically; the dispatcher looks the ticket up here,
    /// removes it (single-use), and trusts the bound identity for
    /// that one request.  Cookie delivery replaces the older
    /// `?access_token=` query path — same ticket lifecycle, no URL
    /// surface to leak via proxy access logs / Referer.
    pub(crate) sse_tickets: std::sync::Mutex<HashMap<String, SseTicket>>,
    /// Live artefact-ingest target (URL + bearer).  `None` when swarm
    /// hasn't pushed one yet — `send_artefact` skips the push in
    /// that case.  Mutated only by the configure-push handler; cloned
    /// snapshot is taken on each emit so the lock window is tight.
    /// One shared `Arc` so per-turn `SseOutput` can clone a handle
    /// without re-locking the chats map.
    pub(crate) ingest: Arc<std::sync::Mutex<Option<IngestConfig>>>,
    /// `true` when the HTTP listener terminates TLS itself.  Drives
    /// the `Secure` cookie attribute on the SSE ticket cookie so
    /// browsers refuse to send it over plain HTTP.  Loopback dev
    /// (`dangerous_no_tls`) sets this to `false` so the cookie is
    /// still usable on `http://127.0.0.1:7878`.
    pub(crate) tls_enabled: bool,
    /// Swarm latches this via `/api/admin/quiesce` before it snapshots
    /// the cube for a template rotation. New `/turn` requests are
    /// refused while true so no fresh transcript write lands after the
    /// snapshot moment and disappears during the pointer swap.
    pub(crate) quiesced: std::sync::atomic::AtomicBool,
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

/// Per-process artefact-ingest target.  Pushed by swarm via
/// `/api/admin/configure` (Stage 8 posture: cube's snapshot/restore
/// freezes `/proc/self/environ`, so the matching `SWARM_INGEST_*`
/// env vars only land on the warmup-time process — same root cause
/// the `proxy_token` / `proxy_base` configure-push exists for).
///
/// Read by `SseOutput::send_artefact` on every emit; missing fields
/// (URL or token empty) signal "ingest disabled" and the push is
/// skipped.  None on a fresh boot before the first configure-push.
#[derive(Clone, Debug)]
pub(crate) struct IngestConfig {
    pub(crate) url: String,
    pub(crate) token: String,
}

/// Scan the chat directory (files + archives + artefact metadata) for
/// the highest `c-NNNN` ever used.  Ensures a new chat id never reuses
/// a slot that another record still points at — otherwise the empty
/// new chat would surface orphan artefacts filtered by the old id.
pub(crate) fn max_chat_id_n(data_dir: &std::path::Path, artefacts: &ArtefactStore) -> u64 {
    let mut max_n: u64 = 0;
    if let Ok(iter) = std::fs::read_dir(data_dir) {
        for entry in iter.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && let Some(n) = chat_id_n(name)
            {
                max_n = max_n.max(n);
            }
        }
    }
    // Artefacts retain the owning chat_id even when the chat file has
    // been purged — rotation leaves them orphaned on disk.  Walk the
    // in-memory index (already hydrated from disk) for c-NNNN hits.
    for entry in artefacts.items.values() {
        if let Some(n) = chat_id_n(&entry.chat_id) {
            max_n = max_n.max(n);
        }
    }
    max_n
}

fn chat_id_n(name: &str) -> Option<u64> {
    let stem = name.strip_prefix("c-")?;
    let digits: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

fn prefixed_numeric_id(stem: &str, prefix: char) -> Option<u64> {
    stem.strip_prefix(prefix)
        .and_then(|rest| rest.parse::<u64>().ok())
}

fn file_id_from_name(name: &str) -> Option<u64> {
    let stem = name
        .strip_suffix(".meta.json")
        .or_else(|| name.strip_suffix(".bin"))?;
    prefixed_numeric_id(stem, 'f')
}

fn artefact_id_from_name(name: &str) -> Option<u64> {
    let stem = name
        .strip_suffix(".meta.json")
        .or_else(|| name.strip_suffix(".body"))?;
    prefixed_numeric_id(stem, 'a')
}

fn advance_counter(counter: &std::sync::atomic::AtomicU64, next: u64) {
    let mut current = counter.load(std::sync::atomic::Ordering::Relaxed);
    while current < next {
        match counter.compare_exchange(
            current,
            next,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}

impl HttpState {
    // Constructor shape mirrors the controller's startup pipeline 1:1 —
    // every arg is a distinct lifetime-bound dependency.  A builder would
    // hide that, not simplify it.
    #[allow(clippy::too_many_arguments)]
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
        tls_enabled: bool,
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

        let activity = Arc::new(crate::controller::ActivityRegistry::new(data_dir.clone()));

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
            tls_enabled,
            titles: std::sync::Mutex::new(HashMap::new()),
            allowed_identity: std::sync::Mutex::new(allowed_identity),
            // Warmup-time defaults from the env envelope.  Cube's
            // snapshot/restore freezes /proc/self/environ at the
            // template-build boot — these reads return empty strings
            // for swarm-managed instances until /api/admin/configure
            // patches in the live values.  A non-swarm dyson (terminal
            // / telegram only) sees both empty and skips the push.
            ingest: {
                let url = std::env::var("SWARM_INGEST_URL").unwrap_or_default();
                let token = std::env::var("SWARM_INGEST_TOKEN").unwrap_or_default();
                let cfg = if !url.is_empty() && !token.is_empty() {
                    Some(IngestConfig { url, token })
                } else {
                    None
                };
                Arc::new(std::sync::Mutex::new(cfg))
            },
            quiesced: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(crate) fn is_quiesced(&self) -> bool {
        self.quiesced.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn unquiesce(&self) {
        self.quiesced
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Swarm replays durable files after the HTTP state has already been
    /// constructed, so startup scans cannot see those restored ids.  Bump
    /// the live counters as each accepted state file lands to keep future
    /// chat/file/artefact ids globally unique for the process.
    pub(crate) fn observe_replayed_state_file(&self, namespace: &str, rel_path: &str) {
        if namespace != "chats" {
            return;
        }
        let parts: Vec<&str> = rel_path.split('/').filter(|p| !p.is_empty()).collect();
        if let Some(first) = parts.first().copied()
            && let Some(n) = chat_id_n(first)
        {
            advance_counter(&self.next_id, n.saturating_add(1));
        }

        match parts.as_slice() {
            ["files", name] => {
                if let Some(n) = file_id_from_name(name) {
                    advance_counter(&self.file_id, n.saturating_add(1));
                }
            }
            [_, "files", name] => {
                if let Some(n) = file_id_from_name(name) {
                    advance_counter(&self.file_id, n.saturating_add(1));
                }
            }
            [_, "artefacts", name] => {
                if let Some(n) = artefact_id_from_name(name) {
                    advance_counter(&self.artefact_id, n.saturating_add(1));
                }
            }
            _ => {}
        }
    }

    /// Evict cached per-chat agents that have been idle longer than
    /// `max_idle`.  The agent is dropped (freeing its full in-memory
    /// history, including any restored base64 images); the next turn for
    /// that chat rebuilds it transparently from the on-disk transcript
    /// (the `guard.is_none()` branch of the turn dispatcher).
    ///
    /// Only chats with no active turn are considered: `busy` gates
    /// admission, and `try_lock` skips any agent a racing turn already
    /// holds.  Returns the number of agents evicted.
    pub(crate) async fn evict_idle_agents(&self, max_idle: std::time::Duration) -> usize {
        let handles: Vec<Arc<ChatHandle>> = self.chats.lock().await.values().cloned().collect();
        let mut evicted = 0usize;
        for handle in handles {
            if handle.busy.load(std::sync::atomic::Ordering::SeqCst) {
                continue;
            }
            if handle.idle_for() < max_idle {
                continue;
            }
            let Ok(mut guard) = handle.agent.try_lock() else {
                continue; // a turn is racing us — leave it alone
            };
            // Re-check busy under the agent lock: a POST that latched
            // busy between our first check and try_lock is now blocked
            // on this very lock and expects the agent state it left.
            if handle.busy.load(std::sync::atomic::Ordering::SeqCst) {
                continue;
            }
            if guard.take().is_some() {
                evicted += 1;
                tracing::info!(chat_id = %handle.chat_id, "evicted idle per-chat agent");
            }
        }
        evicted
    }

    pub(crate) async fn in_flight_chats(&self) -> u32 {
        let chats = self.chats.lock().await;
        let count = chats
            .values()
            .filter(|h| h.busy.load(std::sync::atomic::Ordering::SeqCst))
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    pub(crate) async fn try_quiesce(&self) -> u32 {
        self.quiesced
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let in_flight = self.in_flight_chats().await;
        if in_flight != 0 {
            self.unquiesce();
        }
        in_flight
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
        // Drop the sse_tickets guard before acquiring allowed_identity:
        // any code path that takes both must walk the locks in the same
        // order, and this is the only site that touches both.  Holding
        // the ticket lock across an unrelated lock acquisition is the
        // classic recipe for a future deadlock when someone adds a
        // helper that takes them in the other order.
        let entry = {
            let mut guard = match self.sse_tickets.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let entry = guard.remove(token)?;
            if entry.expires_at <= std::time::Instant::now() {
                return None;
            }
            entry
        };
        let allowed = match self.allowed_identity.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        };
        if let Some(allowed) = allowed
            && !ct_eq_identity(&entry.identity, &allowed)
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
    /// `/api/providers` without restarting the controller.  Also
    /// reloads the registry so the test mirrors the production
    /// hot-reload order used by `command::listen`.
    #[doc(hidden)]
    pub fn replace_settings_for_test(&self, settings: Settings) {
        self.registry.reload(&settings, None);
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

    /// Path to dyson.json resolved at startup.  None when launched
    /// without --config (in-memory only).  Used by the
    /// `/api/admin/configure` route to patch the agent's model list
    /// in-place; HotReloader watches this path and rebuilds the
    /// agent on the next turn.
    pub(crate) fn config_path(&self) -> Option<&std::path::Path> {
        self.config_path.as_deref()
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
            self.file_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let file_url = format!("/api/conversations/{chat_id}/files/{file_id}");
        let file_entry = FileEntry {
            bytes,
            mime: mime.clone(),
            name: name.clone(),
            chat_id: chat_id.to_string(),
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

/// Constant-time equality for the SSE ticket identity check.
/// OIDC `sub` is not a high-entropy bearer, but using `ct_eq` parity
/// with the rest of the bearer/secret comparisons in this codebase
/// keeps the door closed against a future change in identity shape
/// introducing a timing oracle here.
fn ct_eq_identity(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    bool::from(a.ct_eq(b))
}

#[cfg(test)]
mod eviction_tests {
    use super::*;
    use crate::agent::Agent;

    struct NoopLlm;

    #[async_trait::async_trait]
    impl crate::llm::LlmClient for NoopLlm {
        async fn stream(
            &self,
            _messages: &[Message],
            _system: &str,
            _system_suffix: &str,
            _tools: &[crate::llm::ToolDefinition],
            _config: &crate::llm::CompletionConfig,
        ) -> crate::error::Result<crate::llm::StreamResponse> {
            Err(crate::error::DysonError::Llm("noop".into()))
        }
    }

    fn make_agent() -> Agent {
        let settings = crate::config::AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        };
        let sandbox: Arc<dyn crate::sandbox::Sandbox> =
            Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
                crate::sandbox::SandboxBypassGuard::for_test(),
            ));
        Agent::new(
            crate::agent::rate_limiter::RateLimitedHandle::unlimited(
                Box::new(NoopLlm) as Box<dyn crate::llm::LlmClient>
            ),
            sandbox,
            Vec::new(),
            &settings,
            None,
            0,
            None,
            None,
        )
        .unwrap()
    }

    fn build_test_state() -> Arc<HttpState> {
        let settings = Settings::default();
        let registry = Arc::new(ClientRegistry::new(&settings, None));
        super::super::test_helpers::build_state(
            settings,
            registry,
            None,
            None,
            Arc::new(crate::auth::DangerousNoAuth),
        )
    }

    // Regression: the per-chat Agent cache never evicted — every chat
    // ever opened kept its full in-memory history (including restored
    // base64 images) until deletion or process restart.
    #[tokio::test]
    async fn idle_agents_are_evicted_but_busy_and_fresh_ones_are_kept() {
        let state = build_test_state();

        let idle = Arc::new(ChatHandle::new("c-idle".into(), "t".into(), None));
        *idle.agent.lock().await = Some(make_agent());
        let busy = Arc::new(ChatHandle::new("c-busy".into(), "t".into(), None));
        *busy.agent.lock().await = Some(make_agent());
        busy.busy.store(true, std::sync::atomic::Ordering::SeqCst);
        let fresh = Arc::new(ChatHandle::new("c-fresh".into(), "t".into(), None));
        *fresh.agent.lock().await = Some(make_agent());

        {
            let mut chats = state.chats.lock().await;
            chats.insert("c-idle".into(), Arc::clone(&idle));
            chats.insert("c-busy".into(), Arc::clone(&busy));
            chats.insert("c-fresh".into(), Arc::clone(&fresh));
        }

        // Age the idle and busy chats past the threshold; keep `fresh`
        // recent by touching it.
        let past = std::time::Instant::now() - std::time::Duration::from_secs(3600);
        *idle.last_used.lock().unwrap() = past;
        *busy.last_used.lock().unwrap() = past;
        fresh.touch();

        let evicted = state
            .evict_idle_agents(std::time::Duration::from_secs(30 * 60))
            .await;

        assert_eq!(evicted, 1, "exactly the idle, non-busy agent is evicted");
        assert!(
            idle.agent.lock().await.is_none(),
            "idle agent must be dropped — the next turn rebuilds it from disk \
             (the guard.is_none() branch of the turn dispatcher)"
        );
        assert!(
            busy.agent.lock().await.is_some(),
            "a chat with an in-flight turn must never lose its agent"
        );
        assert!(
            fresh.agent.lock().await.is_some(),
            "recently-used agents stay cached"
        );

        // Idempotent: a second sweep finds nothing new.
        assert_eq!(
            state
                .evict_idle_agents(std::time::Duration::from_secs(30 * 60))
                .await,
            0
        );
    }
}

#[cfg(test)]
mod ct_eq_identity_tests {
    use super::ct_eq_identity;

    #[test]
    fn ct_eq_identity_matches_equal_strings() {
        assert!(ct_eq_identity("alice@example.com", "alice@example.com"));
        assert!(ct_eq_identity("", ""));
    }

    #[test]
    fn ct_eq_identity_rejects_mismatched_strings() {
        assert!(!ct_eq_identity("alice", "bob"));
        assert!(!ct_eq_identity("alice", "alic"));
        assert!(!ct_eq_identity("alice", "alicee"));
    }
}
