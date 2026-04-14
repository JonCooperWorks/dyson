//! Hub-side task state tracking.
//!
//! The `TaskStore` unifies state for both synchronous dispatches (via the
//! blocking `swarm_dispatch` MCP tool) and asynchronous dispatches (via
//! `swarm_submit`).  Every dispatched task lives here from creation until
//! it is reaped.
//!
//! For sync dispatches the record carries a `oneshot::Sender<SwarmResult>`
//! — the `waiter` — which the result handler fires when the node reports
//! back.  Async dispatches store `waiter: None`; the caller retrieves the
//! result by polling `swarm_task_result`.
//!
//! This module owns the *only* store for in-flight task state on the hub.
//! Before its introduction the hub kept a separate `pending_dispatches`
//! map that could race with the result handler; consolidating everything
//! behind one lock eliminates that race.

pub mod persistence;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dyson_swarm_protocol::types::{SwarmResult, TaskCheckpoint, TaskStatus};
use serde::Serialize;
use tokio::sync::{RwLock, oneshot};

use crate::tasks::persistence::TaskPersistence;

// ---------------------------------------------------------------------------
// TaskState — hub-internal lifecycle for a task
// ---------------------------------------------------------------------------

/// The lifecycle state a task can be in.
///
/// The terminal variants (`Completed`, `Failed`, `Cancelled`) mirror the
/// protocol-level `TaskStatus`, but as a hub-internal enum we can also
/// carry the intermediate `Running` state.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaskState {
    /// Task has been handed to the node and is executing.
    Running,
    /// Task finished successfully.
    Completed,
    /// Task failed.  Error string is the node's report.
    Failed { error: String },
    /// Task was cancelled (reserved — no cancellation path in v1).
    Cancelled,
}

impl TaskState {
    /// True for Completed, Failed, and Cancelled.
    pub const fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running)
    }

    fn from_task_status(status: &TaskStatus) -> Self {
        match status {
            TaskStatus::Completed => Self::Completed,
            TaskStatus::Failed { error } => Self::Failed {
                error: error.clone(),
            },
            TaskStatus::Cancelled => Self::Cancelled,
        }
    }
}

// ---------------------------------------------------------------------------
// TaskRecord — live state for one task
// ---------------------------------------------------------------------------

/// Full state for a task kept on the hub.
///
/// The `waiter` field is the hub's private handle back to a blocking
/// `swarm_dispatch` caller; it is `None` for async submissions.
pub struct TaskRecord {
    pub task_id: String,
    pub node_id: String,
    /// Verified identity of the MCP caller that submitted this task.
    /// Set from the bearer-token → node_id lookup.  `None` for tasks
    /// submitted without authentication (admin / legacy callers).
    pub owner: Option<String>,
    pub prompt_preview: String,
    pub submitted_at: SystemTime,
    pub last_update: SystemTime,
    pub state: TaskState,
    pub checkpoints: Vec<TaskCheckpoint>,
    pub result: Option<SwarmResult>,
    /// Set only for a sync dispatcher waiting on its oneshot.  Taken by
    /// `finalize` (or `abandon_waiter`) and never re-inserted.
    pub waiter: Option<oneshot::Sender<SwarmResult>>,
}

// ---------------------------------------------------------------------------
// TaskSnapshot — lock-free view for read-side consumers (MCP tools)
// ---------------------------------------------------------------------------

/// An immutable snapshot of a `TaskRecord`, safe to return from
/// read-side MCP handlers without holding any locks.  Does not carry
/// the oneshot waiter.
#[derive(Debug, Clone, Serialize)]
pub struct TaskSnapshot {
    pub task_id: String,
    pub node_id: String,
    pub owner: Option<String>,
    pub prompt_preview: String,
    pub submitted_at_unix: u64,
    pub last_update_unix: u64,
    pub state: TaskState,
    pub checkpoints: Vec<TaskCheckpoint>,
    pub result: Option<SwarmResult>,
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl TaskSnapshot {
    fn from_record(r: &TaskRecord) -> Self {
        Self {
            task_id: r.task_id.clone(),
            node_id: r.node_id.clone(),
            owner: r.owner.clone(),
            prompt_preview: r.prompt_preview.clone(),
            submitted_at_unix: unix_secs(r.submitted_at),
            last_update_unix: unix_secs(r.last_update),
            state: r.state.clone(),
            checkpoints: r.checkpoints.clone(),
            result: r.result.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// TaskStore — thread-safe map of task records
// ---------------------------------------------------------------------------

/// Thread-safe store of all known tasks on the hub.
///
/// Single `RwLock` over the whole map.  Fine for v1 (expect <100 concurrent
/// tasks); shard per-task_id if it becomes a hot spot.
///
/// Every mutation writes through to a `TaskPersistence` backend so tasks
/// survive hub restarts.  The in-memory map is the hot read path; the
/// persistence layer is the durable ledger.
#[derive(Clone)]
pub struct TaskStore {
    inner: Arc<RwLock<HashMap<String, TaskRecord>>>,
    persistence: Arc<dyn TaskPersistence>,
}

impl TaskStore {
    /// Build a new task store backed by the given persistence layer.
    ///
    /// `recovered` should be the result of `persistence.load_all()` —
    /// these records are loaded into the in-memory map without being
    /// re-persisted.
    pub fn with_persistence(
        persistence: Arc<dyn TaskPersistence>,
        recovered: Vec<TaskRecord>,
    ) -> Self {
        let mut map = HashMap::with_capacity(recovered.len());
        for record in recovered {
            map.insert(record.task_id.clone(), record);
        }
        Self {
            inner: Arc::new(RwLock::new(map)),
            persistence,
        }
    }

    /// Create a task store backed by an in-memory SQLite database.
    /// Used by tests.
    pub async fn new_for_test() -> Self {
        let p = persistence::SqliteTaskPersistence::open_in_memory()
            .await
            .expect("failed to open in-memory SQLite for test");
        Self::with_persistence(Arc::new(p), Vec::new())
    }

    /// Insert a new task record.  Overwrites any existing entry for the
    /// same task_id — unique IDs are the caller's responsibility (we use
    /// UUIDv4 in `mcp.rs`).
    pub async fn insert(&self, record: TaskRecord) {
        let task_id = record.task_id.clone();
        {
            let mut inner = self.inner.write().await;
            inner.insert(record.task_id.clone(), record);
        }
        // Persist outside the write lock.  Re-read under a read lock
        // since record was moved into the map.
        let inner = self.inner.read().await;
        if let Some(record) = inner.get(&task_id)
            && let Err(e) = self.persistence.insert(record).await {
                tracing::error!(task_id = %task_id, error = %e, "failed to persist task insert");
            }
    }

    /// Append a checkpoint to the named task.  Returns `false` if the
    /// task is unknown or already terminal (late checkpoints after a
    /// final result are dropped).
    pub async fn append_checkpoint(&self, cp: TaskCheckpoint) -> bool {
        let task_id = cp.task_id.clone();
        let accepted = {
            let mut inner = self.inner.write().await;
            match inner.get_mut(&cp.task_id) {
                Some(record) if !record.state.is_terminal() => {
                    record.last_update = SystemTime::now();
                    record.checkpoints.push(cp.clone());
                    true
                }
                _ => false,
            }
        };
        if accepted
            && let Err(e) = self.persistence.append_checkpoint(&task_id, &cp).await {
                tracing::error!(task_id = %task_id, error = %e, "failed to persist checkpoint");
            }
        accepted
    }

    /// Finalize a task with the given result.
    ///
    /// Sets the state from `result.status`, stores the result, and takes
    /// the oneshot waiter out of the record so the caller can fire it
    /// outside any lock.  Returns `None` if the task is unknown or if
    /// there was no waiter (async submission).
    ///
    /// If the task is already in terminal state `Cancelled` (because
    /// `cancel()` was called before this result arrived), the state is
    /// left as `Cancelled` but the node's actual result is still stored
    /// for debugging.  Other terminal states are similarly preserved on
    /// a second finalize — first writer wins.
    pub async fn finalize(
        &self,
        task_id: &str,
        result: SwarmResult,
    ) -> Option<oneshot::Sender<SwarmResult>> {
        let (waiter, state) = {
            let mut inner = self.inner.write().await;
            let record = inner.get_mut(task_id)?;
            if !record.state.is_terminal() {
                record.state = TaskState::from_task_status(&result.status);
            }
            record.last_update = SystemTime::now();
            record.result = Some(result.clone());
            (record.waiter.take(), record.state.clone())
        };
        if let Err(e) = self.persistence.finalize(task_id, &result, &state).await {
            tracing::error!(task_id = %task_id, error = %e, "failed to persist task finalize");
        }
        waiter
    }

    /// Clear the waiter without touching state/result.  Used when the
    /// sync dispatcher times out or its channel closes — the record
    /// stays so a late result can still land, but we no longer try to
    /// fire a dead oneshot.
    pub async fn abandon_waiter(&self, task_id: &str) {
        let mut inner = self.inner.write().await;
        if let Some(record) = inner.get_mut(task_id) {
            record.waiter = None;
        }
    }

    /// Mark a running task as Cancelled and return info about it.
    ///
    /// Returns `Some((node_id, waiter))` on success — the waiter is the
    /// sync dispatcher's oneshot (if any), which the caller should fire
    /// with a synthetic Cancelled `SwarmResult` so a blocking
    /// `swarm_dispatch` returns promptly.  The node_id lets the caller
    /// push a `CancelTask` SSE event to the owning node.
    ///
    /// Returns `None` if the task is unknown or already terminal.
    ///
    /// A late `finalize` call from the node after cancellation will
    /// find the task in a terminal state and update the record's
    /// `result` with whatever the node actually reported — handy for
    /// debugging which callback ran last — while leaving the state as
    /// whatever the node decided (Cancelled, Completed, Failed).  For
    /// async callers that never poll again, the Cancelled state
    /// recorded here is the final word.
    pub async fn cancel(
        &self,
        task_id: &str,
    ) -> Option<(String, Option<oneshot::Sender<SwarmResult>>)> {
        let synthetic_result = SwarmResult {
            task_id: task_id.to_string(),
            text: String::new(),
            payloads: vec![],
            status: TaskStatus::Cancelled,
            duration_secs: 0,
        };
        let ret = {
            let mut inner = self.inner.write().await;
            let record = inner.get_mut(task_id)?;
            if record.state.is_terminal() {
                return None;
            }
            record.state = TaskState::Cancelled;
            record.last_update = SystemTime::now();
            record.result = Some(synthetic_result.clone());
            Some((record.node_id.clone(), record.waiter.take()))
        };
        if let Err(e) = self.persistence.cancel(task_id, &synthetic_result).await {
            tracing::error!(task_id = %task_id, error = %e, "failed to persist task cancel");
        }
        ret
    }

    /// Return a snapshot of one task if it exists.
    pub async fn get(&self, task_id: &str) -> Option<TaskSnapshot> {
        let inner = self.inner.read().await;
        inner.get(task_id).map(TaskSnapshot::from_record)
    }

    /// Return the checkpoints of one task whose sequence is strictly
    /// greater than `since`.  Returns `None` if the task is unknown.
    pub async fn checkpoints_since(
        &self,
        task_id: &str,
        since: u32,
    ) -> Option<Vec<TaskCheckpoint>> {
        let inner = self.inner.read().await;
        let record = inner.get(task_id)?;
        Some(
            record
                .checkpoints
                .iter()
                .filter(|c| c.sequence > since)
                .cloned()
                .collect(),
        )
    }

    /// List tasks newest-first, up to `limit`.
    pub async fn list(&self, limit: usize) -> Vec<TaskSnapshot> {
        let inner = self.inner.read().await;
        let mut snaps: Vec<TaskSnapshot> =
            inner.values().map(TaskSnapshot::from_record).collect();
        snaps.sort_by(|a, b| b.submitted_at_unix.cmp(&a.submitted_at_unix));
        snaps.truncate(limit);
        snaps
    }

    // -----------------------------------------------------------------------
    // Owner-scoped queries — topology obfuscation: non-owned tasks are
    // invisible (same as nonexistent) so callers cannot distinguish
    // "task belongs to someone else" from "task does not exist".
    // -----------------------------------------------------------------------

    /// Return a snapshot only if the task is owned by `owner`.
    pub async fn get_owned(&self, task_id: &str, owner: &str) -> Option<TaskSnapshot> {
        let inner = self.inner.read().await;
        inner
            .get(task_id)
            .filter(|r| r.owner.as_deref() == Some(owner))
            .map(TaskSnapshot::from_record)
    }

    /// List tasks owned by `owner`, newest-first, up to `limit`.
    pub async fn list_owned(&self, owner: &str, limit: usize) -> Vec<TaskSnapshot> {
        let inner = self.inner.read().await;
        let mut snaps: Vec<TaskSnapshot> = inner
            .values()
            .filter(|r| r.owner.as_deref() == Some(owner))
            .map(TaskSnapshot::from_record)
            .collect();
        snaps.sort_by(|a, b| b.submitted_at_unix.cmp(&a.submitted_at_unix));
        snaps.truncate(limit);
        snaps
    }

    /// Return checkpoints for a task only if owned by `owner`.
    pub async fn checkpoints_since_owned(
        &self,
        task_id: &str,
        since: u32,
        owner: &str,
    ) -> Option<Vec<TaskCheckpoint>> {
        let inner = self.inner.read().await;
        let record = inner.get(task_id)?;
        if record.owner.as_deref() != Some(owner) {
            return None;
        }
        Some(
            record
                .checkpoints
                .iter()
                .filter(|c| c.sequence > since)
                .cloned()
                .collect(),
        )
    }

    /// Cancel a task only if owned by `owner`.  Returns `None` for
    /// unknown, non-owned, or already-terminal tasks.
    pub async fn cancel_owned(
        &self,
        task_id: &str,
        owner: &str,
    ) -> Option<(String, Option<oneshot::Sender<SwarmResult>>)> {
        let synthetic_result = SwarmResult {
            task_id: task_id.to_string(),
            text: String::new(),
            payloads: vec![],
            status: TaskStatus::Cancelled,
            duration_secs: 0,
        };
        let ret = {
            let mut inner = self.inner.write().await;
            let record = inner.get_mut(task_id)?;
            if record.owner.as_deref() != Some(owner) {
                return None;
            }
            if record.state.is_terminal() {
                return None;
            }
            record.state = TaskState::Cancelled;
            record.last_update = SystemTime::now();
            record.result = Some(synthetic_result.clone());
            Some((record.node_id.clone(), record.waiter.take()))
        };
        if ret.is_some()
            && let Err(e) = self.persistence.cancel(task_id, &synthetic_result).await {
                tracing::error!(task_id = %task_id, error = %e, "failed to persist task cancel");
            }
        ret
    }

    /// How many tasks are in the store.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// True when the store holds no tasks.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }

    /// Drop any terminal task whose `last_update` is older than `ttl`.
    /// Returns the number reaped.
    pub async fn reap(&self, ttl: Duration) -> usize {
        let now = SystemTime::now();
        let to_remove: Vec<String> = {
            let inner = self.inner.read().await;
            inner
                .iter()
                .filter_map(|(id, record)| {
                    if record.state.is_terminal()
                        && now
                            .duration_since(record.last_update)
                            .map(|d| d > ttl)
                            .unwrap_or(false)
                    {
                        Some(id.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };
        if to_remove.is_empty() {
            return 0;
        }
        let n = to_remove.len();
        {
            let mut inner = self.inner.write().await;
            for id in &to_remove {
                inner.remove(id);
            }
        }
        for id in &to_remove {
            if let Err(e) = self.persistence.remove(id).await {
                tracing::error!(task_id = %id, error = %e, "failed to persist task removal");
            }
        }
        n
    }
}

// ---------------------------------------------------------------------------
// Startup recovery helpers
// ---------------------------------------------------------------------------

/// Error string used when a task is found in `Running` state after a hub
/// restart.  Exposed so tests and operators can grep for it.
pub const ORPHANED_RUNNING_ERROR: &str = "hub restarted mid-task";

/// Reconcile tasks recovered from persistence after a hub restart.
///
/// Any task still in `Running` state belongs to a node whose SSE session
/// died with the old process and which has lost its `node_id`/token — the
/// hub cannot talk to it anymore.  Such tasks are marked `Failed` with a
/// stable error string, both in the caller-owned in-memory slice *and* in
/// the durable store, so `swarm_task_result` reports a terminal state
/// instead of lying that the task is still running.
///
/// Returns the number of records that were flipped.
pub async fn reconcile_orphaned_running(
    persistence: &dyn TaskPersistence,
    recovered: &mut [TaskRecord],
) -> Result<usize, sqlx::Error> {
    let mut count = 0;
    for record in recovered.iter_mut() {
        if !matches!(record.state, TaskState::Running) {
            continue;
        }
        let result = SwarmResult {
            task_id: record.task_id.clone(),
            text: String::new(),
            payloads: vec![],
            status: TaskStatus::Failed {
                error: ORPHANED_RUNNING_ERROR.to_string(),
            },
            duration_secs: 0,
        };
        let new_state = TaskState::Failed {
            error: ORPHANED_RUNNING_ERROR.to_string(),
        };
        persistence
            .finalize(&record.task_id, &result, &new_state)
            .await?;
        record.state = new_state;
        record.result = Some(result);
        record.last_update = SystemTime::now();
        count += 1;
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dyson_swarm_protocol::types::TaskStatus;

    fn blank_record(task_id: &str) -> TaskRecord {
        TaskRecord {
            task_id: task_id.into(),
            node_id: "node-a".into(),
            owner: None,
            prompt_preview: "do a thing".into(),
            submitted_at: SystemTime::now(),
            last_update: SystemTime::now(),
            state: TaskState::Running,
            checkpoints: Vec::new(),
            result: None,
            waiter: None,
        }
    }

    fn owned_record(task_id: &str, owner: &str) -> TaskRecord {
        TaskRecord {
            owner: Some(owner.into()),
            ..blank_record(task_id)
        }
    }

    fn sample_result(task_id: &str, status: TaskStatus) -> SwarmResult {
        SwarmResult {
            task_id: task_id.into(),
            text: "ok".into(),
            payloads: vec![],
            status,
            duration_secs: 1,
        }
    }

    #[tokio::test]
    async fn insert_then_get_roundtrip() {
        let store = TaskStore::new_for_test().await;
        store.insert(blank_record("t1")).await;
        let snap = store.get("t1").await.unwrap();
        assert_eq!(snap.task_id, "t1");
        assert!(matches!(snap.state, TaskState::Running));
        assert!(snap.checkpoints.is_empty());
    }

    #[tokio::test]
    async fn append_checkpoint_preserves_order_and_is_filtered_by_since() {
        let store = TaskStore::new_for_test().await;
        store.insert(blank_record("t1")).await;
        for seq in 1..=3 {
            store
                .append_checkpoint(TaskCheckpoint {
                    task_id: "t1".into(),
                    sequence: seq,
                    message: format!("step {seq}"),
                    progress: None,
                    emitted_at_secs: seq as u64,
                })
                .await;
        }
        let snap = store.get("t1").await.unwrap();
        assert_eq!(snap.checkpoints.len(), 3);
        assert_eq!(snap.checkpoints[0].sequence, 1);
        assert_eq!(snap.checkpoints[2].sequence, 3);

        let since_1 = store.checkpoints_since("t1", 1).await.unwrap();
        assert_eq!(since_1.len(), 2);
        assert_eq!(since_1[0].sequence, 2);
    }

    #[tokio::test]
    async fn checkpoints_on_unknown_task_returns_false() {
        let store = TaskStore::new_for_test().await;
        let ok = store
            .append_checkpoint(TaskCheckpoint {
                task_id: "nope".into(),
                sequence: 1,
                message: "hi".into(),
                progress: None,
                emitted_at_secs: 0,
            })
            .await;
        assert!(!ok);
    }

    #[tokio::test]
    async fn finalize_returns_waiter_and_stores_result() {
        let store = TaskStore::new_for_test().await;
        let (tx, rx) = oneshot::channel::<SwarmResult>();
        let mut rec = blank_record("t1");
        rec.waiter = Some(tx);
        store.insert(rec).await;

        let result = sample_result("t1", TaskStatus::Completed);
        let waiter = store.finalize("t1", result.clone()).await;
        assert!(waiter.is_some());
        waiter.unwrap().send(result.clone()).unwrap();

        let got = rx.await.unwrap();
        assert_eq!(got.task_id, "t1");

        let snap = store.get("t1").await.unwrap();
        assert!(matches!(snap.state, TaskState::Completed));
        assert!(snap.result.is_some());
    }

    #[tokio::test]
    async fn finalize_without_waiter_still_stores_result() {
        let store = TaskStore::new_for_test().await;
        store.insert(blank_record("t1")).await;

        let waiter = store
            .finalize("t1", sample_result("t1", TaskStatus::Completed))
            .await;
        assert!(waiter.is_none());

        let snap = store.get("t1").await.unwrap();
        assert!(matches!(snap.state, TaskState::Completed));
        assert!(snap.result.is_some());
    }

    #[tokio::test]
    async fn failed_task_state_carries_error() {
        let store = TaskStore::new_for_test().await;
        store.insert(blank_record("t1")).await;
        store
            .finalize(
                "t1",
                sample_result(
                    "t1",
                    TaskStatus::Failed {
                        error: "boom".into(),
                    },
                ),
            )
            .await;
        let snap = store.get("t1").await.unwrap();
        match snap.state {
            TaskState::Failed { error } => assert_eq!(error, "boom"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn abandon_waiter_clears_it_without_finalizing() {
        let store = TaskStore::new_for_test().await;
        let (tx, _rx) = oneshot::channel::<SwarmResult>();
        let mut rec = blank_record("t1");
        rec.waiter = Some(tx);
        store.insert(rec).await;

        store.abandon_waiter("t1").await;

        // A later finalize returns None because the waiter was already cleared.
        let waiter = store
            .finalize("t1", sample_result("t1", TaskStatus::Completed))
            .await;
        assert!(waiter.is_none());
    }

    #[tokio::test]
    async fn cancel_running_task_sets_cancelled_and_returns_waiter() {
        let store = TaskStore::new_for_test().await;
        let (tx, rx) = oneshot::channel::<SwarmResult>();
        let mut rec = blank_record("t1");
        rec.waiter = Some(tx);
        store.insert(rec).await;

        let (node_id, waiter) = store.cancel("t1").await.unwrap();
        assert_eq!(node_id, "node-a");
        assert!(waiter.is_some());

        // Fire the waiter with a synthetic Cancelled result so any sync
        // dispatcher unblocks.
        waiter
            .unwrap()
            .send(sample_result("t1", TaskStatus::Cancelled))
            .unwrap();
        let r = rx.await.unwrap();
        assert!(matches!(r.status, TaskStatus::Cancelled));

        let snap = store.get("t1").await.unwrap();
        assert!(matches!(snap.state, TaskState::Cancelled));
        assert!(snap.result.is_some());
    }

    #[tokio::test]
    async fn cancel_unknown_or_terminal_returns_none() {
        let store = TaskStore::new_for_test().await;
        assert!(store.cancel("nope").await.is_none());

        store.insert(blank_record("t1")).await;
        store
            .finalize("t1", sample_result("t1", TaskStatus::Completed))
            .await;
        assert!(store.cancel("t1").await.is_none());
    }

    #[tokio::test]
    async fn late_checkpoint_after_terminal_is_dropped() {
        let store = TaskStore::new_for_test().await;
        store.insert(blank_record("t1")).await;
        store
            .finalize("t1", sample_result("t1", TaskStatus::Completed))
            .await;
        let ok = store
            .append_checkpoint(TaskCheckpoint {
                task_id: "t1".into(),
                sequence: 99,
                message: "stale".into(),
                progress: None,
                emitted_at_secs: 0,
            })
            .await;
        assert!(!ok);
        assert!(store.get("t1").await.unwrap().checkpoints.is_empty());
    }

    #[tokio::test]
    async fn list_sorts_newest_first_and_respects_limit() {
        let store = TaskStore::new_for_test().await;
        let base = SystemTime::now();
        for i in 0..5 {
            let mut rec = blank_record(&format!("t{i}"));
            rec.submitted_at = base + Duration::from_secs(i);
            store.insert(rec).await;
        }
        let snaps = store.list(3).await;
        assert_eq!(snaps.len(), 3);
        assert_eq!(snaps[0].task_id, "t4");
        assert_eq!(snaps[1].task_id, "t3");
        assert_eq!(snaps[2].task_id, "t2");
    }

    // -----------------------------------------------------------------------
    // Ownership-scoped query tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn get_owned_returns_none_for_wrong_owner() {
        let store = TaskStore::new_for_test().await;
        store.insert(owned_record("t1", "alice")).await;
        assert!(store.get_owned("t1", "bob").await.is_none());
    }

    #[tokio::test]
    async fn get_owned_returns_snapshot_for_correct_owner() {
        let store = TaskStore::new_for_test().await;
        store.insert(owned_record("t1", "alice")).await;
        let snap = store.get_owned("t1", "alice").await.unwrap();
        assert_eq!(snap.task_id, "t1");
    }

    #[tokio::test]
    async fn get_owned_returns_none_for_unowned_task() {
        let store = TaskStore::new_for_test().await;
        store.insert(blank_record("t1")).await; // owner: None
        assert!(store.get_owned("t1", "alice").await.is_none());
    }

    #[tokio::test]
    async fn list_owned_filters_by_owner() {
        let store = TaskStore::new_for_test().await;
        let base = SystemTime::now();
        for i in 0..3 {
            let mut rec = owned_record(&format!("a{i}"), "alice");
            rec.submitted_at = base + Duration::from_secs(i);
            store.insert(rec).await;
        }
        for i in 0..2 {
            let mut rec = owned_record(&format!("b{i}"), "bob");
            rec.submitted_at = base + Duration::from_secs(10 + i);
            store.insert(rec).await;
        }
        store.insert(blank_record("unowned")).await;

        let alice_tasks = store.list_owned("alice", 50).await;
        assert_eq!(alice_tasks.len(), 3);
        assert!(alice_tasks.iter().all(|t| t.owner.as_deref() == Some("alice")));

        let bob_tasks = store.list_owned("bob", 50).await;
        assert_eq!(bob_tasks.len(), 2);

        let nobody_tasks = store.list_owned("nobody", 50).await;
        assert!(nobody_tasks.is_empty());
    }

    #[tokio::test]
    async fn checkpoints_since_owned_rejects_wrong_owner() {
        let store = TaskStore::new_for_test().await;
        store.insert(owned_record("t1", "alice")).await;
        store
            .append_checkpoint(TaskCheckpoint {
                task_id: "t1".into(),
                sequence: 1,
                message: "hi".into(),
                progress: None,
                emitted_at_secs: 0,
            })
            .await;
        assert!(store.checkpoints_since_owned("t1", 0, "bob").await.is_none());
        let cps = store.checkpoints_since_owned("t1", 0, "alice").await.unwrap();
        assert_eq!(cps.len(), 1);
    }

    #[tokio::test]
    async fn cancel_owned_rejects_wrong_owner() {
        let store = TaskStore::new_for_test().await;
        store.insert(owned_record("t1", "alice")).await;
        assert!(store.cancel_owned("t1", "bob").await.is_none());
        // Task should still be running.
        let snap = store.get("t1").await.unwrap();
        assert!(matches!(snap.state, TaskState::Running));
        // Correct owner can cancel.
        let (node_id, _) = store.cancel_owned("t1", "alice").await.unwrap();
        assert_eq!(node_id, "node-a");
    }

    #[tokio::test]
    async fn reap_removes_only_terminal_tasks_past_ttl() {
        let store = TaskStore::new_for_test().await;

        // terminal, old → reaped
        let mut old = blank_record("old");
        old.state = TaskState::Completed;
        old.last_update = SystemTime::now() - Duration::from_secs(3600);
        store.insert(old).await;

        // terminal, fresh → kept
        let mut fresh = blank_record("fresh");
        fresh.state = TaskState::Completed;
        store.insert(fresh).await;

        // running, old → kept
        let mut running = blank_record("running");
        running.last_update = SystemTime::now() - Duration::from_secs(3600);
        store.insert(running).await;

        let reaped = store.reap(Duration::from_secs(60)).await;
        assert_eq!(reaped, 1);
        assert!(store.get("old").await.is_none());
        assert!(store.get("fresh").await.is_some());
        assert!(store.get("running").await.is_some());
    }

    #[tokio::test]
    async fn reconcile_orphaned_running_flips_running_to_failed_and_persists() {
        use crate::tasks::persistence::SqliteTaskPersistence;

        let persistence = Arc::new(SqliteTaskPersistence::open_in_memory().await.unwrap());

        // Seed three records: one Running (should be reconciled), one
        // already Completed (should be untouched), and one already
        // Failed with a different error (should be untouched).
        let running = blank_record("running-1");
        let mut completed = blank_record("completed-1");
        completed.state = TaskState::Completed;
        let mut failed = blank_record("failed-1");
        failed.state = TaskState::Failed {
            error: "original boom".into(),
        };

        for r in [&running, &completed, &failed] {
            persistence.insert(r).await.unwrap();
        }

        // Simulate a fresh load_all() after restart.
        let mut recovered = persistence.load_all().await.unwrap();
        let flipped = reconcile_orphaned_running(persistence.as_ref(), &mut recovered)
            .await
            .unwrap();
        assert_eq!(flipped, 1);

        // In-memory view is consistent.
        let mut by_id: std::collections::HashMap<String, &TaskRecord> = Default::default();
        for r in &recovered {
            by_id.insert(r.task_id.clone(), r);
        }
        match &by_id["running-1"].state {
            TaskState::Failed { error } => assert_eq!(error, ORPHANED_RUNNING_ERROR),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(by_id["running-1"].result.is_some());
        assert!(matches!(by_id["completed-1"].state, TaskState::Completed));
        match &by_id["failed-1"].state {
            TaskState::Failed { error } => assert_eq!(error, "original boom"),
            other => panic!("expected Failed(original boom), got {other:?}"),
        }

        // Durable store reflects the same flip (reload to be sure).
        let reloaded = persistence.load_all().await.unwrap();
        let reloaded_running = reloaded
            .iter()
            .find(|r| r.task_id == "running-1")
            .expect("running-1 present");
        match &reloaded_running.state {
            TaskState::Failed { error } => assert_eq!(error, ORPHANED_RUNNING_ERROR),
            other => panic!("expected persisted Failed, got {other:?}"),
        }

        // Second reconcile pass is a no-op.
        let mut recovered2 = persistence.load_all().await.unwrap();
        let flipped2 = reconcile_orphaned_running(persistence.as_ref(), &mut recovered2)
            .await
            .unwrap();
        assert_eq!(flipped2, 0);
    }
}
