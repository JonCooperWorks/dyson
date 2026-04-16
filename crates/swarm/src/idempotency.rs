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
//! Memory discipline: entries expire after `TTL`, and an opportunistic
//! sweep runs on insert once the map grows past `SOFT_CAP`.  No separate
//! background task — the cost is amortized over writers.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

/// How long an idempotency record is kept.  Matches the 24h task TTL —
/// once the referenced task has been reaped from the task store the
/// mapping can't be replayed into anything useful.
const TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Soft cap above which an insert triggers a sweep of expired entries.
/// Picked to keep steady-state memory predictable for a busy hub without
/// sweeping on every single write.
const SOFT_CAP: usize = 4_096;

/// Max length of a caller-supplied idempotency key.  Keys this long are
/// already excessive; rejecting anything larger is a simple abuse guard.
pub const MAX_KEY_LEN: usize = 256;

type Key = (String, String); // (owner_id, idempotency_key)

pub struct IdempotencyIndex {
    inner: RwLock<HashMap<Key, (String, Instant)>>,
}

impl IdempotencyIndex {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// If `(owner, key)` already has a fresh mapping, return its task_id.
    /// Otherwise insert `new_task_id` and return `None`.  Atomic under
    /// the index's write lock — two concurrent submits with the same
    /// key see deterministic first-writer-wins behavior.
    pub async fn check_or_insert(
        &self,
        owner: &str,
        key: &str,
        new_task_id: &str,
    ) -> Option<String> {
        let now = Instant::now();
        let k = (owner.to_string(), key.to_string());
        let mut map = self.inner.write().await;

        if let Some((existing, inserted_at)) = map.get(&k)
            && now.duration_since(*inserted_at) < TTL
        {
            return Some(existing.clone());
        }

        if map.len() >= SOFT_CAP {
            map.retain(|_, (_, at)| now.duration_since(*at) < TTL);
        }

        map.insert(k, (new_task_id.to_string(), now));
        None
    }
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
    async fn first_insert_returns_none() {
        let idx = IdempotencyIndex::new();
        assert!(idx.check_or_insert("owner-a", "k1", "task-1").await.is_none());
    }

    #[tokio::test]
    async fn duplicate_returns_existing_task_id() {
        let idx = IdempotencyIndex::new();
        assert!(idx.check_or_insert("owner-a", "k1", "task-1").await.is_none());
        let got = idx.check_or_insert("owner-a", "k1", "task-2").await;
        assert_eq!(got.as_deref(), Some("task-1"));
    }

    #[tokio::test]
    async fn same_key_different_owner_is_isolated() {
        let idx = IdempotencyIndex::new();
        idx.check_or_insert("owner-a", "k1", "task-a").await;
        let got = idx.check_or_insert("owner-b", "k1", "task-b").await;
        assert!(got.is_none(), "owner-b key should be independent");
    }
}
