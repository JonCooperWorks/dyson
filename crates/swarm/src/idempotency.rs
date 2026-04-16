//! Idempotency index for `swarm_submit`.
//!
//! Maps `(owner_id, client-supplied key)` → (task_id, inserted-at) so that
//! a retried MCP call with the same key returns the original task_id
//! instead of dispatching a new task.  Pure in-memory: a hub restart
//! clears the map, and that is acceptable — restarts are rare, and a
//! caller that retained the `task_id` from the first call can always
//! poll `swarm_task_result` directly.
//!
//! The map is scoped per owner so two nodes (or a node and an API-key
//! caller) can reuse the same key string without colliding.
//!
//! Memory discipline: entries expire after `TTL`.  A background sweep
//! task runs on a fixed interval to evict expired entries even when the
//! hub is write-silent, and an opportunistic sweep also fires on insert
//! once the map grows past `SOFT_CAP` (covers bursty writers between
//! background ticks).
//!
//! Recording policy: the contract is "record successful submissions".
//! `reserve` takes a write lock, checks for a fresh duplicate, and if
//! none exists inserts a *tentative* entry.  The caller must then call
//! `commit` once the task is accepted into the store, or `rollback` if
//! dispatch fails — otherwise a phantom mapping would pin the key to a
//! task_id that was never created.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

/// How long a committed idempotency record is kept.  Matches the 24h
/// task TTL so committed entries decay alongside the task they point at.
const TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Tentative entries (reserved, not yet committed) expire fast — the
/// caller either commits within milliseconds or rolls back.  A stale
/// tentative entry is almost certainly a dropped `place_task` that
/// never called `rollback`; freeing it quickly lets a retry proceed.
const TENTATIVE_TTL: Duration = Duration::from_secs(30);

/// Soft cap above which an insert triggers a sweep of expired entries.
/// Picked to keep steady-state memory predictable for a busy hub without
/// sweeping on every single write.
const SOFT_CAP: usize = 4_096;

/// Max length of a caller-supplied idempotency key.  Keys this long are
/// already excessive; rejecting anything larger is a simple abuse guard.
pub const MAX_KEY_LEN: usize = 256;

type Key = (String, String); // (owner_id, idempotency_key)

/// Outcome of `reserve`.
pub enum Reservation {
    /// A fresh mapping was created.  Dispatch the task, then call
    /// `commit` on this reservation's key, or `rollback` on failure.
    Fresh(ReservationHandle),
    /// A prior, committed mapping already exists — replay it.
    Replay(String),
}

/// Handle returned when `reserve` creates a tentative entry.  Keeps the
/// (owner, key) pair so the caller can finalize atomically.
pub struct ReservationHandle {
    key: Key,
}

impl ReservationHandle {
    pub fn key(&self) -> &(String, String) {
        &self.key
    }
}

#[derive(Clone)]
struct Entry {
    task_id: String,
    inserted_at: Instant,
    committed: bool,
}

pub struct IdempotencyIndex {
    inner: RwLock<HashMap<Key, Entry>>,
}

impl IdempotencyIndex {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Reserve `(owner, key)` for `tentative_task_id`.
    ///
    /// - If a committed entry exists and is within TTL, returns
    ///   `Reservation::Replay(existing_task_id)`.
    /// - Otherwise inserts a tentative entry and returns
    ///   `Reservation::Fresh(handle)`.  The caller MUST call `commit`
    ///   after a successful dispatch or `rollback` on failure; a lost
    ///   reservation is eventually swept by `TENTATIVE_TTL`.
    ///
    /// Atomic under the write lock — concurrent submits with the same
    /// key see deterministic first-writer-wins behavior.
    pub async fn reserve(
        &self,
        owner: &str,
        key: &str,
        tentative_task_id: &str,
    ) -> Reservation {
        let now = Instant::now();
        let k = (owner.to_string(), key.to_string());
        let mut map = self.inner.write().await;

        if let Some(e) = map.get(&k) {
            let fresh = if e.committed {
                now.duration_since(e.inserted_at) < TTL
            } else {
                now.duration_since(e.inserted_at) < TENTATIVE_TTL
            };
            if fresh {
                if e.committed {
                    return Reservation::Replay(e.task_id.clone());
                }
                // An in-flight reservation exists — treat as a replay of
                // the tentative id.  The caller that raced in second will
                // receive the same id the first caller is about to commit
                // (or roll back).  This matches the first-writer-wins
                // invariant without letting a duplicate dispatch slip in.
                return Reservation::Replay(e.task_id.clone());
            }
        }

        if map.len() >= SOFT_CAP {
            map.retain(|_, e| !is_expired(e, now));
        }

        map.insert(
            k.clone(),
            Entry {
                task_id: tentative_task_id.to_string(),
                inserted_at: now,
                committed: false,
            },
        );
        Reservation::Fresh(ReservationHandle { key: k })
    }

    /// Promote a tentative reservation to committed.  Called after the
    /// task has been accepted into the task store.
    pub async fn commit(&self, handle: ReservationHandle) {
        let mut map = self.inner.write().await;
        if let Some(e) = map.get_mut(&handle.key) {
            e.committed = true;
            e.inserted_at = Instant::now();
        }
    }

    /// Drop a tentative reservation.  Called when `place_task` fails so
    /// a retry with the same key can proceed.  Idempotent; a no-op if
    /// the entry was already swept.
    pub async fn rollback(&self, handle: ReservationHandle) {
        let mut map = self.inner.write().await;
        // Only drop if the entry is still ours and still tentative —
        // we never want to evict a committed mapping.
        if let Some(e) = map.get(&handle.key)
            && !e.committed
        {
            map.remove(&handle.key);
        }
    }

    /// One-shot sweep of expired entries.  Cheap under normal load.
    pub async fn sweep(&self) {
        let now = Instant::now();
        let mut map = self.inner.write().await;
        map.retain(|_, e| !is_expired(e, now));
    }
}

fn is_expired(e: &Entry, now: Instant) -> bool {
    let ttl = if e.committed { TTL } else { TENTATIVE_TTL };
    now.duration_since(e.inserted_at) >= ttl
}

impl Default for IdempotencyIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_reserve_is_fresh() {
        let idx = IdempotencyIndex::new();
        assert!(matches!(
            idx.reserve("owner-a", "k1", "task-1").await,
            Reservation::Fresh(_)
        ));
    }

    #[tokio::test]
    async fn committed_duplicate_replays() {
        let idx = IdempotencyIndex::new();
        let h = match idx.reserve("owner-a", "k1", "task-1").await {
            Reservation::Fresh(h) => h,
            _ => panic!("expected fresh"),
        };
        idx.commit(h).await;
        let r = idx.reserve("owner-a", "k1", "task-2").await;
        match r {
            Reservation::Replay(id) => assert_eq!(id, "task-1"),
            _ => panic!("expected replay"),
        }
    }

    #[tokio::test]
    async fn rollback_allows_retry() {
        let idx = IdempotencyIndex::new();
        let h = match idx.reserve("owner-a", "k1", "task-1").await {
            Reservation::Fresh(h) => h,
            _ => panic!("expected fresh"),
        };
        idx.rollback(h).await;
        // Retry must be a fresh reservation, not a replay of a dead id.
        assert!(matches!(
            idx.reserve("owner-a", "k1", "task-2").await,
            Reservation::Fresh(_)
        ));
    }

    #[tokio::test]
    async fn same_key_different_owner_is_isolated() {
        let idx = IdempotencyIndex::new();
        let _ = idx.reserve("owner-a", "k1", "task-a").await;
        assert!(matches!(
            idx.reserve("owner-b", "k1", "task-b").await,
            Reservation::Fresh(_)
        ));
    }

    #[tokio::test]
    async fn concurrent_reservation_sees_replay_of_tentative() {
        let idx = IdempotencyIndex::new();
        let _h = match idx.reserve("owner-a", "k1", "task-1").await {
            Reservation::Fresh(h) => h,
            _ => panic!("expected fresh"),
        };
        // Second caller, before the first commits or rolls back:
        let r = idx.reserve("owner-a", "k1", "task-2").await;
        match r {
            Reservation::Replay(id) => assert_eq!(id, "task-1"),
            _ => panic!("expected replay of tentative"),
        }
    }
}
