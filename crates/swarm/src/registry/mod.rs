//! Node registry with write-through persistence.
//!
//! Every registered node has an entry keyed by `node_id` that holds:
//!
//! - the manifest sent at registration time
//! - the current `NodeStatus` (reported via heartbeats, or mutated by
//!   the dispatcher when a task is handed off)
//! - the bearer token we issued at registration — we look this up on
//!   every authed request to resolve it back to a `node_id`
//! - the mpsc sender that feeds the node's SSE stream
//! - the timestamps of the last heartbeat (monotonic for the reaper,
//!   wall-clock for MCP readers)
//!
//! The in-memory `HashMap` is the hot read path.  Every mutation writes
//! through to a `NodePersistence` backend (SQLite by default) so the
//! roster survives hub restarts.  On startup `load_all()` rehydrates the
//! persisted rows and `reconcile_recovered_nodes` flips each to
//! `Draining`, forcing the dispatcher to refuse new work until the node
//! proves it is still alive via a fresh heartbeat.  In v1 that never
//! happens — a reconnecting node re-registers from scratch and gets a
//! new `node_id`/token pair — so stale recovered rows are eventually
//! dropped by the reaper.

pub mod persistence;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dyson_swarm_protocol::types::{NodeManifest, NodeStatus};
use tokio::sync::{RwLock, mpsc};

use crate::registry::persistence::{NodePersistence, PersistedNode};

/// Alias to keep the node_id type explicit.
pub type NodeId = String;

/// Events pushed to a node's SSE channel.
///
/// The HTTP layer converts these into `text/event-stream` frames.
#[derive(Debug, Clone)]
pub enum SseEvent {
    /// Initial handshake — sent by the events handler when the stream opens.
    Registered { node_id: String },
    /// Signed task wire bytes (version || signature || payload).
    Task(Vec<u8>),
    /// Heartbeat acknowledgement.
    HeartbeatAck,
    /// Cancel a running task.  The node checks whether it's currently
    /// executing this task_id and, if so, cancels the per-task token
    /// so the agent loop drops the in-flight work.
    CancelTask(String),
    /// Graceful shutdown request.
    Shutdown,
}

/// One entry in the registry.
pub struct NodeEntry {
    pub node_id: NodeId,
    pub token: String,
    pub manifest: NodeManifest,
    pub status: NodeStatus,
    /// Monotonic timestamp, used by the reaper for stale-node eviction.
    pub last_heartbeat: Instant,
    /// Wall-clock timestamp, stamped alongside `last_heartbeat` so MCP
    /// callers can see "when did this node last check in" as a Unix
    /// second value. `Instant` cannot be converted to Unix time, so we
    /// carry both.
    pub last_heartbeat_at: SystemTime,
    /// None until the node opens its SSE stream.
    pub sse_tx: Option<mpsc::Sender<SseEvent>>,
}

impl NodeEntry {
    /// Project into the on-disk representation.
    fn to_persisted(&self) -> PersistedNode {
        PersistedNode {
            node_id: self.node_id.clone(),
            token: self.token.clone(),
            manifest: self.manifest.clone(),
            status: self.status.clone(),
            last_heartbeat_at: self.last_heartbeat_at,
        }
    }
}

/// Thread-safe node registry with write-through SQLite persistence.
#[derive(Clone)]
pub struct NodeRegistry {
    inner: Arc<RwLock<Inner>>,
    persistence: Arc<dyn NodePersistence>,
}

pub(crate) struct Inner {
    pub(crate) by_id: HashMap<NodeId, NodeEntry>,
    // Reverse index so token→node_id is O(1).
    pub(crate) token_to_id: HashMap<String, NodeId>,
}

fn unix_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl NodeRegistry {
    /// Build a new registry backed by the given persistence layer.
    ///
    /// `recovered` should be the result of `persistence.load_all()`
    /// (optionally post-processed by `reconcile_recovered_nodes`); these
    /// entries are loaded into the in-memory map without being
    /// re-persisted.  Callers are responsible for forcing the status of
    /// recovered entries to `Draining` before passing them in.
    pub fn with_persistence(
        persistence: Arc<dyn NodePersistence>,
        recovered: Vec<PersistedNode>,
    ) -> Self {
        let mut by_id: HashMap<NodeId, NodeEntry> = HashMap::with_capacity(recovered.len());
        let mut token_to_id: HashMap<String, NodeId> = HashMap::with_capacity(recovered.len());
        // Monotonic clocks reset across restarts, so the best we can do
        // is start every recovered node's reap clock from "now".  This
        // gives a reconnecting node the full heartbeat-timeout window
        // to come back before it is reaped.
        let now = Instant::now();
        for node in recovered {
            token_to_id.insert(node.token.clone(), node.node_id.clone());
            by_id.insert(
                node.node_id.clone(),
                NodeEntry {
                    node_id: node.node_id,
                    token: node.token,
                    manifest: node.manifest,
                    status: node.status,
                    last_heartbeat: now,
                    last_heartbeat_at: node.last_heartbeat_at,
                    sse_tx: None,
                },
            );
        }
        Self {
            inner: Arc::new(RwLock::new(Inner { by_id, token_to_id })),
            persistence,
        }
    }

    /// Build a registry backed by an in-memory SQLite database.  Used by tests.
    pub async fn new_for_test() -> Self {
        let p = persistence::SqliteNodePersistence::open_in_memory()
            .await
            .expect("failed to open in-memory SQLite for test");
        Self::with_persistence(Arc::new(p), Vec::new())
    }

    /// Register a new node, returning the assigned (node_id, token).
    pub async fn register(&self, manifest: NodeManifest) -> (NodeId, String) {
        let node_id = uuid::Uuid::new_v4().to_string();
        let token = crate::auth::generate_token();

        let entry = NodeEntry {
            node_id: node_id.clone(),
            token: token.clone(),
            status: manifest.status.clone(),
            manifest,
            last_heartbeat: Instant::now(),
            last_heartbeat_at: SystemTime::now(),
            sse_tx: None,
        };

        let persisted = entry.to_persisted();

        {
            let mut inner = self.inner.write().await;
            inner.by_id.insert(node_id.clone(), entry);
            inner.token_to_id.insert(token.clone(), node_id.clone());
        }

        // Persist outside the write lock so readers are not blocked by I/O.
        if let Err(e) = self.persistence.insert(&persisted).await {
            tracing::error!(node_id = %node_id, error = %e, "failed to persist node register");
        }

        (node_id, token)
    }

    /// Resolve a bearer token back to a node_id.
    pub async fn node_id_for_token(&self, token: &str) -> Option<NodeId> {
        self.inner.read().await.token_to_id.get(token).cloned()
    }

    /// Attach an SSE sender to the node, replacing any previous one.
    ///
    /// Not persisted — the channel is runtime-only.
    pub async fn attach_sse(&self, node_id: &str, tx: mpsc::Sender<SseEvent>) {
        if let Some(entry) = self.inner.write().await.by_id.get_mut(node_id) {
            entry.sse_tx = Some(tx);
        }
    }

    /// Detach a node's SSE sender, typically when the client disconnects.
    ///
    /// Not persisted — the channel is runtime-only.
    pub async fn detach_sse(&self, node_id: &str) {
        if let Some(entry) = self.inner.write().await.by_id.get_mut(node_id) {
            entry.sse_tx = None;
        }
    }

    /// Remove a node from the registry entirely, clearing both the id
    /// index and the token index.  Returns `true` if the node existed.
    ///
    /// Called when the node's SSE stream disconnects: the operator
    /// expects a disconnected node to vanish immediately, not linger as
    /// a stale entry until the reaper catches up.  A reconnecting node
    /// simply re-registers and gets a fresh id+token.
    pub async fn remove_node(&self, node_id: &str) -> bool {
        let existed = {
            let mut inner = self.inner.write().await;
            if let Some(entry) = inner.by_id.remove(node_id) {
                inner.token_to_id.remove(&entry.token);
                true
            } else {
                false
            }
        };
        if existed
            && let Err(e) = self.persistence.remove(node_id).await {
                tracing::error!(node_id = %node_id, error = %e, "failed to persist node removal");
            }
        existed
    }

    /// Push an event to the node's SSE stream if one is attached.
    ///
    /// Returns `false` if the node is unknown or has no SSE sender, or if
    /// the send failed because the receiver was dropped.
    pub async fn push_event(&self, node_id: &str, event: SseEvent) -> bool {
        let tx_opt = {
            let inner = self.inner.read().await;
            inner
                .by_id
                .get(node_id)
                .and_then(|e| e.sse_tx.clone())
        };
        match tx_opt {
            Some(tx) => tx.send(event).await.is_ok(),
            None => false,
        }
    }

    /// Record a heartbeat from the given node and update its status.
    pub async fn heartbeat(&self, node_id: &str, status: NodeStatus) -> bool {
        let persist_args = {
            let mut inner = self.inner.write().await;
            if let Some(entry) = inner.by_id.get_mut(node_id) {
                let now_wall = SystemTime::now();
                entry.last_heartbeat = Instant::now();
                entry.last_heartbeat_at = now_wall;
                entry.status = status.clone();
                Some((status, unix_secs(now_wall)))
            } else {
                None
            }
        };
        match persist_args {
            Some((status, hb_unix)) => {
                if let Err(e) = self
                    .persistence
                    .update_status(node_id, &status, hb_unix)
                    .await
                {
                    tracing::error!(node_id = %node_id, error = %e, "failed to persist node heartbeat");
                }
                true
            }
            None => false,
        }
    }

    /// Set the node's status without touching its heartbeat timestamp.
    ///
    /// Used by the dispatcher to flip `Idle → Busy` when handing off a task.
    /// The on-disk `last_heartbeat_unix` is rewritten unchanged so the row
    /// stays consistent.
    pub async fn set_status(&self, node_id: &str, status: NodeStatus) -> bool {
        let persist_args = {
            let mut inner = self.inner.write().await;
            if let Some(entry) = inner.by_id.get_mut(node_id) {
                entry.status = status.clone();
                Some((status, unix_secs(entry.last_heartbeat_at)))
            } else {
                None
            }
        };
        match persist_args {
            Some((status, hb_unix)) => {
                if let Err(e) = self
                    .persistence
                    .update_status(node_id, &status, hb_unix)
                    .await
                {
                    tracing::error!(node_id = %node_id, error = %e, "failed to persist node status");
                }
                true
            }
            None => false,
        }
    }

    /// Remove any node whose last heartbeat is older than `timeout`.
    ///
    /// Returns the list of reaped node_ids so callers can log them.
    pub async fn reap_stale(&self, timeout: Duration) -> Vec<NodeId> {
        let now = Instant::now();
        let mut reaped = Vec::new();
        {
            let mut inner = self.inner.write().await;
            let stale_ids: Vec<NodeId> = inner
                .by_id
                .iter()
                .filter_map(|(id, entry)| {
                    if now.saturating_duration_since(entry.last_heartbeat) > timeout {
                        Some(id.clone())
                    } else {
                        None
                    }
                })
                .collect();
            for id in stale_ids {
                if let Some(entry) = inner.by_id.remove(&id) {
                    inner.token_to_id.remove(&entry.token);
                    reaped.push(id);
                }
            }
        }
        // Persist the removals outside the write lock.
        for id in &reaped {
            if let Err(e) = self.persistence.remove(id).await {
                tracing::error!(node_id = %id, error = %e, "failed to persist node reap");
            }
        }
        reaped
    }

    /// Run a read-only closure against the inner map.
    ///
    /// This is the ergonomic way for the router and MCP layers to inspect
    /// all nodes without holding the lock themselves.
    pub async fn with_entries<R>(&self, f: impl FnOnce(&HashMap<NodeId, NodeEntry>) -> R) -> R {
        let inner = self.inner.read().await;
        f(&inner.by_id)
    }

    /// Run a read-only closure against a single entry keyed by `node_id`.
    ///
    /// Returns `None` if the node doesn't exist. Used by the
    /// caller-directed dispatch path to validate `target_node_id`
    /// without cloning the entry.
    pub async fn with_entry<R>(
        &self,
        node_id: &str,
        f: impl FnOnce(&NodeEntry) -> R,
    ) -> Option<R> {
        let inner = self.inner.read().await;
        inner.by_id.get(node_id).map(f)
    }

    /// Test-only: grab a write lock on the inner map for direct manipulation.
    #[cfg(test)]
    pub(crate) async fn inner_for_test(
        &self,
    ) -> tokio::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().await
    }

    /// Count entries grouped by status — used by the `swarm_status` MCP tool.
    pub async fn counts(&self) -> RegistryCounts {
        let inner = self.inner.read().await;
        let mut counts = RegistryCounts::default();
        for entry in inner.by_id.values() {
            counts.total += 1;
            match entry.status {
                NodeStatus::Idle => counts.idle += 1,
                NodeStatus::Busy { .. } => counts.busy += 1,
                NodeStatus::Draining => {}
            }
        }
        counts
    }
}

/// Counts used by the `swarm_status` MCP tool.
#[derive(Debug, Default, Clone, Copy)]
pub struct RegistryCounts {
    pub total: usize,
    pub idle: usize,
    pub busy: usize,
}

// ---------------------------------------------------------------------------
// Startup recovery helpers
// ---------------------------------------------------------------------------

/// Reconcile nodes recovered from persistence after a hub restart.
///
/// Every recovered node has `sse_tx: None` and a fresh monotonic
/// `last_heartbeat` (the old one came from a dead process and cannot be
/// translated).  The hub cannot actually push events to these nodes until
/// they re-open an SSE stream, so we flip every recovered entry to
/// `NodeStatus::Draining` — the router already refuses to dispatch new
/// tasks to a draining node.  The first real heartbeat from a
/// reconnecting node overwrites the status back to whatever the node
/// reports.
///
/// In v1 reconnecting nodes re-register from scratch (fresh `node_id` +
/// token), so stale recovered rows never receive that heartbeat and are
/// eventually dropped by the reaper.  Returns the count of entries that
/// were touched — always the full length of the slice, mirroring
/// `reconcile_orphaned_running`.
pub fn reconcile_recovered_nodes(recovered: &mut [PersistedNode]) -> usize {
    for node in recovered.iter_mut() {
        node.status = NodeStatus::Draining;
    }
    recovered.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::persistence::SqliteNodePersistence;
    use dyson_swarm_protocol::types::{HardwareInfo, NodeManifest, NodeStatus};

    fn test_manifest(name: &str) -> NodeManifest {
        NodeManifest {
            node_name: name.into(),
            os: "linux".into(),
            hardware: HardwareInfo {
                cpus: vec![],
                gpus: vec![],
                ram_bytes: 16 * 1024 * 1024 * 1024,
                disk_free_bytes: 0,
            },
            capabilities: vec![],
            description: None,
            status: NodeStatus::Idle,
        }
    }

    #[tokio::test]
    async fn register_then_lookup() {
        let reg = NodeRegistry::new_for_test().await;
        let (node_id, token) = reg.register(test_manifest("alpha")).await;

        let looked_up = reg.node_id_for_token(&token).await;
        assert_eq!(looked_up.as_deref(), Some(node_id.as_str()));
    }

    #[tokio::test]
    async fn reap_stale_drops_expired_nodes() {
        let reg = NodeRegistry::new_for_test().await;
        let (node_id, _) = reg.register(test_manifest("beta")).await;

        // Force the heartbeat back in time.
        {
            let mut inner = reg.inner.write().await;
            let entry = inner.by_id.get_mut(&node_id).unwrap();
            entry.last_heartbeat = Instant::now() - Duration::from_secs(300);
        }

        let reaped = reg.reap_stale(Duration::from_secs(90)).await;
        assert_eq!(reaped, vec![node_id.clone()]);

        let counts = reg.counts().await;
        assert_eq!(counts.total, 0);
    }

    #[tokio::test]
    async fn remove_node_clears_both_indices() {
        let reg = NodeRegistry::new_for_test().await;
        let (node_id, token) = reg.register(test_manifest("delta")).await;

        assert!(reg.remove_node(&node_id).await);
        assert_eq!(reg.counts().await.total, 0);
        // Token index must be cleared too, otherwise a reused token
        // would resolve back to a dead node_id.
        assert!(reg.node_id_for_token(&token).await.is_none());

        // Second remove is a no-op.
        assert!(!reg.remove_node(&node_id).await);
    }

    #[tokio::test]
    async fn heartbeat_updates_status() {
        let reg = NodeRegistry::new_for_test().await;
        let (node_id, _) = reg.register(test_manifest("gamma")).await;
        let updated = reg
            .heartbeat(
                &node_id,
                NodeStatus::Busy {
                    task_id: "t-1".into(),
                },
            )
            .await;
        assert!(updated);

        let counts = reg.counts().await;
        assert_eq!(counts.total, 1);
        assert_eq!(counts.busy, 1);
        assert_eq!(counts.idle, 0);
    }

    // -----------------------------------------------------------------------
    // Persistence write-through tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn register_is_durable() {
        let persistence = Arc::new(SqliteNodePersistence::open_in_memory().await.unwrap());
        let reg = NodeRegistry::with_persistence(persistence.clone(), Vec::new());
        let (node_id, token) = reg.register(test_manifest("alpha")).await;

        let loaded = persistence.load_all().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].node_id, node_id);
        assert_eq!(loaded[0].token, token);
        assert_eq!(loaded[0].manifest.node_name, "alpha");
        assert!(matches!(loaded[0].status, NodeStatus::Idle));
    }

    #[tokio::test]
    async fn heartbeat_bumps_last_heartbeat_unix_durably() {
        let persistence = Arc::new(SqliteNodePersistence::open_in_memory().await.unwrap());
        let reg = NodeRegistry::with_persistence(persistence.clone(), Vec::new());
        let (node_id, _) = reg.register(test_manifest("alpha")).await;

        // Backdate the on-disk heartbeat so we can observe the bump.
        let initial = persistence.load_all().await.unwrap();
        let initial_hb = unix_secs(initial[0].last_heartbeat_at);
        persistence
            .update_status(&node_id, &NodeStatus::Idle, initial_hb - 3600)
            .await
            .unwrap();

        reg.heartbeat(
            &node_id,
            NodeStatus::Busy {
                task_id: "t-1".into(),
            },
        )
        .await;

        let loaded = persistence.load_all().await.unwrap();
        let new_hb = unix_secs(loaded[0].last_heartbeat_at);
        assert!(
            new_hb >= initial_hb,
            "heartbeat should have been refreshed on disk (was {new_hb}, expected >= {initial_hb})"
        );
        assert!(matches!(loaded[0].status, NodeStatus::Busy { .. }));
    }

    #[tokio::test]
    async fn remove_node_deletes_row_durably() {
        let persistence = Arc::new(SqliteNodePersistence::open_in_memory().await.unwrap());
        let reg = NodeRegistry::with_persistence(persistence.clone(), Vec::new());
        let (node_id, _) = reg.register(test_manifest("alpha")).await;

        assert!(reg.remove_node(&node_id).await);
        let loaded = persistence.load_all().await.unwrap();
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn reap_stale_persists_removal() {
        let persistence = Arc::new(SqliteNodePersistence::open_in_memory().await.unwrap());
        let reg = NodeRegistry::with_persistence(persistence.clone(), Vec::new());
        let (node_id, _) = reg.register(test_manifest("alpha")).await;

        {
            let mut inner = reg.inner.write().await;
            let entry = inner.by_id.get_mut(&node_id).unwrap();
            entry.last_heartbeat = Instant::now() - Duration::from_secs(300);
        }

        let reaped = reg.reap_stale(Duration::from_secs(90)).await;
        assert_eq!(reaped, vec![node_id.clone()]);

        let loaded = persistence.load_all().await.unwrap();
        assert!(loaded.is_empty(), "reaped node should be gone from disk");
    }

    #[test]
    fn reconcile_recovered_nodes_flips_all_to_draining() {
        let mut recovered = vec![
            PersistedNode {
                node_id: "idle-1".into(),
                token: "tok-1".into(),
                manifest: test_manifest("idle-1"),
                status: NodeStatus::Idle,
                last_heartbeat_at: SystemTime::now(),
            },
            PersistedNode {
                node_id: "busy-1".into(),
                token: "tok-2".into(),
                manifest: test_manifest("busy-1"),
                status: NodeStatus::Busy {
                    task_id: "t-1".into(),
                },
                last_heartbeat_at: SystemTime::now(),
            },
            PersistedNode {
                node_id: "draining-1".into(),
                token: "tok-3".into(),
                manifest: test_manifest("draining-1"),
                status: NodeStatus::Draining,
                last_heartbeat_at: SystemTime::now(),
            },
        ];

        let count = reconcile_recovered_nodes(&mut recovered);
        assert_eq!(count, 3);
        for node in &recovered {
            assert!(matches!(node.status, NodeStatus::Draining));
        }
    }

    #[tokio::test]
    async fn restart_roundtrip_recovers_as_draining() {
        let persistence = Arc::new(SqliteNodePersistence::open_in_memory().await.unwrap());

        // First run: register a node as Idle.
        let (node_id, token) = {
            let reg = NodeRegistry::with_persistence(persistence.clone(), Vec::new());
            let (id, tok) = reg.register(test_manifest("alpha")).await;
            let counts = reg.counts().await;
            assert_eq!(counts.idle, 1);
            (id, tok)
        };
        // First registry dropped — simulate a hub restart.

        // Second run: rehydrate from the same persistence layer.
        let mut recovered = persistence.load_all().await.unwrap();
        assert_eq!(recovered.len(), 1);
        let flipped = reconcile_recovered_nodes(&mut recovered);
        assert_eq!(flipped, 1);

        let reg2 = NodeRegistry::with_persistence(persistence.clone(), recovered);

        // The node is present and its token still resolves...
        let looked_up = reg2.node_id_for_token(&token).await;
        assert_eq!(looked_up.as_deref(), Some(node_id.as_str()));

        // ...but its status is Draining so the router refuses new dispatches.
        let counts = reg2.counts().await;
        assert_eq!(counts.total, 1);
        assert_eq!(counts.idle, 0);
        assert_eq!(counts.busy, 0);

        let got_status = reg2
            .with_entry(&node_id, |e| {
                assert!(e.sse_tx.is_none());
                matches!(e.status, NodeStatus::Draining)
            })
            .await
            .unwrap();
        assert!(got_status, "recovered entry should be Draining");
    }
}
