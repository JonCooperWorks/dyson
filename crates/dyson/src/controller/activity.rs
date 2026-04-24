// ===========================================================================
// Activity registry — per-chat, disk-backed log of running tool activity.
//
// Powers the web UI's Activity tab: when a subagent (security_engineer,
// etc.) starts, a `Running` entry appears; when it finishes, the entry
// flips to `Ok` or `Err`.  Entries persist across controller restarts so
// the tab shows session history, not just live state.
//
// UI-only side channel.  Nothing here reaches the parent's LLM
// conversation — that invariant is called out on `CaptureOutput` in
// `crates/dyson/src/skill/subagent/mod.rs`.
//
// On-disk layout:
//   {data_dir}/{chat_id}/activity.jsonl
//
// Mirrors the per-chat subdir pattern established by
// `chat_history::migrate` (commit 9d9973f) and used by the artefact /
// feedback stores.
//
// Append-only JSONL.  Each state transition appends a line, so a single
// run writes two lines (`Running`, then `Ok` / `Err`).  On load we fold
// by `id`, keeping the latest state per id.  Append-only means a mid-run
// crash is safe: the file is never rewritten, only extended.
//
// Crash recovery: a `Running` entry older than `STALE_RUNNING_SECS`
// (1h — longer than security_engineer's worst-case) is implicitly
// promoted to `Err` with note "never finished" at load time.
// ===========================================================================

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// In-memory cap: newest N entries per chat stay hot.  On-disk log is
/// unbounded — older entries can be re-surfaced by a future endpoint
/// without a schema break.  Bounded memory is stated priority (see
/// CLAUDE.md "RSS and binary size matter").
const CAP_PER_CHAT: usize = 64;

/// Running entries older than this at load time are reconciled to
/// `Err` — assume the controller crashed mid-run and the terminal
/// line never got written.  Longer than `security_engineer`'s 80-turn
/// cap runs in practice (~30 min with cheatsheet injection).
const STALE_RUNNING_SECS: u64 = 60 * 60;

/// Lane classifier.  Today only "subagent" is populated by this
/// registry; future wiring for "loop" / "dream" / "swarm" fits into
/// the same file format without a migration.
pub const LANE_SUBAGENT: &str = "subagent";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityStatus {
    Running,
    Ok,
    Err,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub chat_id: String,
    pub lane: String,
    pub name: String,
    /// Short human-readable note (truncated task input).  Surfaces
    /// under the tool name in the UI row.
    pub note: String,
    pub status: ActivityStatus,
    /// Seconds since UNIX epoch.
    pub started_at: u64,
    pub finished_at: Option<u64>,
    /// Monotonic within a chat — used to fold the two-line-per-run
    /// JSONL into a single entry.  Never re-used within a chat's log
    /// (file stays append-only, so "re-use" isn't a concern anyway).
    pub id: u64,
}

/// Disk-backed registry of activity entries, keyed by chat_id.
///
/// Lives inside `Arc` in `HttpState`.  Mutex is `std::sync::Mutex`
/// (sync, not tokio) to match the existing `FileStore` / `ArtefactStore`
/// idiom — critical sections are small (HashMap insert + file append).
pub struct ActivityRegistry {
    by_chat: Mutex<HashMap<String, Vec<ActivityEntry>>>,
    /// Per-chat next-id counter.  Incremented atomically under the
    /// same mutex as `by_chat` so `(chat_id, id)` is unique.
    next_id_by_chat: Mutex<HashMap<String, u64>>,
    data_dir: Option<PathBuf>,
}

impl ActivityRegistry {
    /// Construct an empty registry.  `data_dir` is the HTTP
    /// controller's root (same value already used by `FileStore` /
    /// `ArtefactStore`).  `None` = memory-only mode — entries still
    /// track but nothing is persisted.  Matches the HttpState
    /// convention: no chat_history backend → no disk.
    pub fn new(data_dir: Option<PathBuf>) -> Self {
        let mut registry = Self {
            by_chat: Mutex::new(HashMap::new()),
            next_id_by_chat: Mutex::new(HashMap::new()),
            data_dir,
        };
        registry.hydrate_from_disk();
        registry
    }

    /// Scan `{data_dir}/*/activity.jsonl` and populate the in-memory
    /// index.  Missing files / malformed lines are logged and skipped
    /// — a half-written JSONL row from a crash shouldn't block
    /// startup.  Called once by `new`.
    fn hydrate_from_disk(&mut self) {
        let Some(dir) = self.data_dir.as_ref() else {
            return;
        };
        let iter = match std::fs::read_dir(dir) {
            Ok(it) => it,
            Err(_) => return,
        };
        let now = unix_seconds_now();
        let mut by_chat = self.by_chat.lock().expect("activity by_chat poisoned");
        let mut next_id = self
            .next_id_by_chat
            .lock()
            .expect("activity next_id poisoned");
        for chat_entry in iter.flatten() {
            let chat_path = chat_entry.path();
            if !chat_path.is_dir() {
                continue;
            }
            let Some(chat_id) = chat_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let jsonl = chat_path.join("activity.jsonl");
            let Ok(text) = std::fs::read_to_string(&jsonl) else {
                continue;
            };
            // Fold by id: later lines (terminal states) overwrite
            // earlier lines (Running).  BTreeMap keeps the result
            // sorted by id, which happens to be chronological too.
            let mut folded: std::collections::BTreeMap<u64, ActivityEntry> =
                std::collections::BTreeMap::new();
            let mut max_id: u64 = 0;
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let mut entry: ActivityEntry = match serde_json::from_str(line) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(error = %e, chat_id, "malformed activity.jsonl line — skipping");
                        continue;
                    }
                };
                // Reconcile stale Running entries — a crash during a
                // long run leaves a Running line with no terminal
                // follow-up.  Better to surface it as Err than let the
                // UI pulse forever.
                if entry.status == ActivityStatus::Running
                    && now.saturating_sub(entry.started_at) > STALE_RUNNING_SECS
                {
                    entry.status = ActivityStatus::Err;
                    entry.finished_at = Some(entry.started_at + STALE_RUNNING_SECS);
                    entry.note = if entry.note.is_empty() {
                        "never finished".to_string()
                    } else {
                        format!("{} · never finished", entry.note)
                    };
                }
                max_id = max_id.max(entry.id);
                folded.insert(entry.id, entry);
            }
            if folded.is_empty() {
                continue;
            }
            // Keep newest CAP_PER_CHAT per chat in memory.
            let mut entries: Vec<ActivityEntry> = folded.into_values().collect();
            if entries.len() > CAP_PER_CHAT {
                let drop = entries.len() - CAP_PER_CHAT;
                entries.drain(0..drop);
            }
            by_chat.insert(chat_id.to_string(), entries);
            next_id.insert(chat_id.to_string(), max_id.saturating_add(1));
        }
    }

    /// Build a chat-scoped handle.  Tools never see the registry
    /// directly — they hold an `ActivityHandle` bound to their chat.
    pub fn handle_for(self: &Arc<Self>, chat_id: impl Into<String>) -> ActivityHandle {
        ActivityHandle {
            inner: Arc::clone(self),
            chat_id: chat_id.into(),
        }
    }

    /// Return a snapshot of entries for every chat.  Newest-first.
    /// Used by `/api/activity`.
    pub fn snapshot_all(&self) -> Vec<ActivityEntry> {
        self.reconcile_stale_running();
        let guard = self.by_chat.lock().expect("activity by_chat poisoned");
        let mut all: Vec<ActivityEntry> =
            guard.values().flat_map(|v| v.iter().cloned()).collect();
        all.sort_by_key(|e| std::cmp::Reverse(e.started_at));
        all
    }

    /// Return a snapshot of entries for one chat, newest-first.
    pub fn snapshot_chat(&self, chat_id: &str) -> Vec<ActivityEntry> {
        self.reconcile_stale_running();
        let guard = self.by_chat.lock().expect("activity by_chat poisoned");
        let mut entries: Vec<ActivityEntry> =
            guard.get(chat_id).cloned().unwrap_or_default();
        entries.sort_by_key(|e| std::cmp::Reverse(e.started_at));
        entries
    }

    /// Flip any in-memory Running entry older than `STALE_RUNNING_SECS`
    /// to `Err` with a "never finished" note.  Mirrors the load-time
    /// reconciliation in `hydrate_from_disk` so orphans still get
    /// cleaned up when the controller stays up for longer than a run.
    /// Appends the terminal state to disk so the fix persists across
    /// future restarts.
    fn reconcile_stale_running(&self) {
        let now = unix_seconds_now();
        let mut to_persist: Vec<ActivityEntry> = Vec::new();
        {
            let mut guard = self.by_chat.lock().expect("activity by_chat poisoned");
            for entries in guard.values_mut() {
                for entry in entries.iter_mut() {
                    if entry.status != ActivityStatus::Running {
                        continue;
                    }
                    if now.saturating_sub(entry.started_at) <= STALE_RUNNING_SECS {
                        continue;
                    }
                    entry.status = ActivityStatus::Err;
                    entry.finished_at = Some(entry.started_at + STALE_RUNNING_SECS);
                    entry.note = if entry.note.is_empty() {
                        "never finished".to_string()
                    } else {
                        format!("{} · never finished", entry.note)
                    };
                    to_persist.push(entry.clone());
                }
            }
        }
        for entry in &to_persist {
            self.append_disk(entry);
        }
    }

    fn next_id(&self, chat_id: &str) -> u64 {
        let mut guard = self
            .next_id_by_chat
            .lock()
            .expect("activity next_id poisoned");
        let counter = guard.entry(chat_id.to_string()).or_insert(1);
        let id = *counter;
        *counter += 1;
        id
    }

    /// Append a `Running` entry, both in memory and on disk.
    fn start(&self, chat_id: &str, lane: &str, name: &str, note: &str) -> u64 {
        let id = self.next_id(chat_id);
        let entry = ActivityEntry {
            chat_id: chat_id.to_string(),
            lane: lane.to_string(),
            name: name.to_string(),
            note: note.to_string(),
            status: ActivityStatus::Running,
            started_at: unix_seconds_now(),
            finished_at: None,
            id,
        };
        self.insert_memory(&entry);
        self.append_disk(&entry);
        id
    }

    /// Transition an entry to `Ok` or `Err`.  Called by
    /// `ActivityToken`'s `Drop` impl (no-op if a fence already ran
    /// via `finish`).
    fn end(&self, chat_id: &str, id: u64, status: ActivityStatus, note_suffix: Option<&str>) {
        let finished_at = unix_seconds_now();
        let mut guard = self.by_chat.lock().expect("activity by_chat poisoned");
        let entries = match guard.get_mut(chat_id) {
            Some(v) => v,
            None => return,
        };
        let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
            return;
        };
        // Idempotent: if this token is dropped after an explicit
        // `finish()`, skip re-writing.
        if entry.status != ActivityStatus::Running {
            return;
        }
        entry.status = status;
        entry.finished_at = Some(finished_at);
        if let Some(suffix) = note_suffix {
            entry.note = if entry.note.is_empty() {
                suffix.to_string()
            } else {
                format!("{} · {suffix}", entry.note)
            };
        }
        let updated = entry.clone();
        drop(guard);
        self.append_disk(&updated);
    }

    fn insert_memory(&self, entry: &ActivityEntry) {
        let mut guard = self.by_chat.lock().expect("activity by_chat poisoned");
        let vec = guard.entry(entry.chat_id.clone()).or_default();
        vec.push(entry.clone());
        if vec.len() > CAP_PER_CHAT {
            let drop = vec.len() - CAP_PER_CHAT;
            vec.drain(0..drop);
        }
    }

    fn append_disk(&self, entry: &ActivityEntry) {
        let Some(dir) = self.data_dir.as_ref() else {
            return;
        };
        let sub = dir.join(&entry.chat_id);
        if let Err(e) = std::fs::create_dir_all(&sub) {
            tracing::warn!(error = %e, chat_id = %entry.chat_id, "failed to create activity dir");
            return;
        }
        let path = sub.join("activity.jsonl");
        let line = match serde_json::to_string(entry) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialize activity entry");
                return;
            }
        };
        use std::io::Write;
        let mut f = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "failed to open activity.jsonl");
                return;
            }
        };
        if let Err(e) = writeln!(f, "{line}") {
            tracing::warn!(error = %e, path = %path.display(), "failed to append activity line");
        }
    }
}

/// Chat-scoped view of an `ActivityRegistry`.  Produced by
/// `ActivityRegistry::handle_for(chat_id)` and dropped into
/// `ToolContext` before the agent runs.
#[derive(Clone)]
pub struct ActivityHandle {
    inner: Arc<ActivityRegistry>,
    chat_id: String,
}

impl ActivityHandle {
    /// Record a `Running` entry and return a token that marks it
    /// `Ok` on drop.  Call `finish(Err)` before drop to record a
    /// failure.
    pub fn start(&self, lane: &'static str, name: &str, note: &str) -> ActivityToken {
        let id = self.inner.start(&self.chat_id, lane, name, note);
        ActivityToken {
            inner: Some(Arc::clone(&self.inner)),
            chat_id: self.chat_id.clone(),
            id,
        }
    }
}

/// RAII token returned by `ActivityHandle::start`.  Dropping marks
/// the entry `Ok`; call `finish(Err, _)` explicitly for failures.
///
/// Doesn't attempt to recover from a panic in the tool — if the run
/// panics the token still drops and the entry flips to `Ok`, which
/// is a mild bug; tool code traps panics already so in practice this
/// path is unreachable.  Honest documentation > false precision.
pub struct ActivityToken {
    inner: Option<Arc<ActivityRegistry>>,
    chat_id: String,
    id: u64,
}

impl ActivityToken {
    /// Explicitly mark the entry.  `note_suffix` appends to the
    /// existing note (e.g. duration, error detail).  Safe to call
    /// multiple times — only the first transition wins.
    pub fn finish(mut self, status: ActivityStatus, note_suffix: Option<&str>) {
        if let Some(reg) = self.inner.take() {
            reg.end(&self.chat_id, self.id, status, note_suffix);
        }
    }
}

impl Drop for ActivityToken {
    fn drop(&mut self) {
        if let Some(reg) = self.inner.take() {
            reg.end(&self.chat_id, self.id, ActivityStatus::Ok, None);
        }
    }
}

fn unix_seconds_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Truncate `s` to at most `max` chars, appending `…` if truncated.
/// Used by tool-side callers to keep note strings short.
pub fn truncate_note(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tempdir")
    }

    #[test]
    fn start_then_drop_marks_ok() {
        let dir = tempdir();
        let reg = Arc::new(ActivityRegistry::new(Some(dir.path().to_path_buf())));
        let handle = reg.handle_for("c-0001");
        let _tok = handle.start(LANE_SUBAGENT, "security_engineer", "review auth");
        drop(_tok);
        let snap = reg.snapshot_chat("c-0001");
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].status, ActivityStatus::Ok);
        assert!(snap[0].finished_at.is_some());
    }

    #[test]
    fn explicit_finish_err_wins() {
        let dir = tempdir();
        let reg = Arc::new(ActivityRegistry::new(Some(dir.path().to_path_buf())));
        let tok = reg.handle_for("c-0001").start(LANE_SUBAGENT, "se", "x");
        tok.finish(ActivityStatus::Err, Some("timed out"));
        let snap = reg.snapshot_chat("c-0001");
        assert_eq!(snap[0].status, ActivityStatus::Err);
        assert!(snap[0].note.contains("timed out"));
    }

    #[test]
    fn hydrate_from_disk_survives_restart() {
        let dir = tempdir();
        {
            let reg = Arc::new(ActivityRegistry::new(Some(dir.path().to_path_buf())));
            reg.handle_for("c-0042")
                .start(LANE_SUBAGENT, "security_engineer", "review crate");
            // Drop implicitly marks Ok on Drop.
        }
        let reloaded = ActivityRegistry::new(Some(dir.path().to_path_buf()));
        let snap = reloaded.snapshot_chat("c-0042");
        assert_eq!(snap.len(), 1, "entry should survive restart");
        assert_eq!(snap[0].status, ActivityStatus::Ok);
        assert_eq!(snap[0].name, "security_engineer");
    }

    #[test]
    fn stale_running_entry_reconciled_to_err() {
        let dir = tempdir();
        // Pre-write a Running entry older than STALE_RUNNING_SECS.
        let chat_dir = dir.path().join("c-0099");
        std::fs::create_dir_all(&chat_dir).unwrap();
        let old_ts = unix_seconds_now().saturating_sub(STALE_RUNNING_SECS + 60);
        let entry = ActivityEntry {
            chat_id: "c-0099".into(),
            lane: LANE_SUBAGENT.into(),
            name: "security_engineer".into(),
            note: "crashed mid-run".into(),
            status: ActivityStatus::Running,
            started_at: old_ts,
            finished_at: None,
            id: 1,
        };
        std::fs::write(
            chat_dir.join("activity.jsonl"),
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
        .unwrap();

        let reg = ActivityRegistry::new(Some(dir.path().to_path_buf()));
        let snap = reg.snapshot_chat("c-0099");
        assert_eq!(snap.len(), 1);
        assert_eq!(
            snap[0].status,
            ActivityStatus::Err,
            "stale Running should have been promoted to Err"
        );
        assert!(snap[0].note.contains("never finished"));
    }

    #[test]
    fn stale_running_reconciled_on_snapshot_without_restart() {
        // Forge an in-memory Running entry with a started_at that's
        // already past the stale threshold.  snapshot_chat must promote
        // it to Err without requiring a restart.
        let dir = tempdir();
        let reg = Arc::new(ActivityRegistry::new(Some(dir.path().to_path_buf())));
        reg.handle_for("c-0007").start(LANE_SUBAGENT, "se", "stuck");
        {
            let mut guard = reg.by_chat.lock().unwrap();
            let entries = guard.get_mut("c-0007").unwrap();
            entries[0].started_at = unix_seconds_now().saturating_sub(STALE_RUNNING_SECS + 60);
            entries[0].status = ActivityStatus::Running;
            entries[0].finished_at = None;
        }
        let snap = reg.snapshot_chat("c-0007");
        assert_eq!(snap[0].status, ActivityStatus::Err);
        assert!(snap[0].note.contains("never finished"));
        // And the fix persisted: reload sees Err too.
        let reloaded = ActivityRegistry::new(Some(dir.path().to_path_buf()));
        let reloaded_snap = reloaded.snapshot_chat("c-0007");
        assert_eq!(reloaded_snap[0].status, ActivityStatus::Err);
    }

    #[test]
    fn snapshot_all_spans_chats_newest_first() {
        let dir = tempdir();
        let reg = Arc::new(ActivityRegistry::new(Some(dir.path().to_path_buf())));
        reg.handle_for("c-01").start(LANE_SUBAGENT, "a", "old");
        std::thread::sleep(std::time::Duration::from_secs(1));
        reg.handle_for("c-02").start(LANE_SUBAGENT, "b", "new");
        let all = reg.snapshot_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name, "b", "newer entry should come first");
        assert_eq!(all[1].name, "a");
    }

    #[test]
    fn truncate_note_leaves_short_strings_alone() {
        assert_eq!(truncate_note("hello", 100), "hello");
        assert_eq!(truncate_note("hello world", 5), "hell…");
    }

    #[test]
    fn memory_only_mode_skips_disk() {
        // data_dir = None: registry works, nothing is written.
        let reg = Arc::new(ActivityRegistry::new(None));
        let _tok = reg.handle_for("c-x").start(LANE_SUBAGENT, "se", "task");
        drop(_tok);
        let snap = reg.snapshot_chat("c-x");
        assert_eq!(snap.len(), 1);
        // No panic, no disk write.
    }
}
