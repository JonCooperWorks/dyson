//! In-memory node registry.
//!
//! Every registered node has an entry keyed by `node_id` that holds:
//!
//! - the manifest sent at registration time
//! - the current `NodeStatus` (reported via heartbeats, or mutated by
//!   the dispatcher when a task is handed off)
//! - the bearer token we issued at registration — we look this up on
//!   every authed request to resolve it back to a `node_id`
//! - the mpsc sender that feeds the node's SSE stream
//! - the timestamp of the last heartbeat — used by the reaper
//!
//! Node state is **ephemeral** in v1.  If the hub restarts, every node
//! has to re-register.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dyson_swarm_protocol::types::{NodeManifest, NodeStatus};
use tokio::sync::{RwLock, mpsc};

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
    /// Graceful shutdown request.
    Shutdown,
}

/// One entry in the registry.
pub struct NodeEntry {
    pub node_id: NodeId,
    pub token: String,
    pub manifest: NodeManifest,
    pub status: NodeStatus,
    pub last_heartbeat: Instant,
    /// None until the node opens its SSE stream.
    pub sse_tx: Option<mpsc::Sender<SseEvent>>,
}

/// Thread-safe in-memory node registry.
#[derive(Clone)]
pub struct NodeRegistry {
    inner: Arc<RwLock<Inner>>,
}

pub(crate) struct Inner {
    pub(crate) by_id: HashMap<NodeId, NodeEntry>,
    // Reverse index so token→node_id is O(1).
    pub(crate) token_to_id: HashMap<String, NodeId>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                by_id: HashMap::new(),
                token_to_id: HashMap::new(),
            })),
        }
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
            sse_tx: None,
        };

        let mut inner = self.inner.write().await;
        inner.by_id.insert(node_id.clone(), entry);
        inner.token_to_id.insert(token.clone(), node_id.clone());

        (node_id, token)
    }

    /// Resolve a bearer token back to a node_id.
    pub async fn node_id_for_token(&self, token: &str) -> Option<NodeId> {
        self.inner.read().await.token_to_id.get(token).cloned()
    }

    /// Attach an SSE sender to the node, replacing any previous one.
    pub async fn attach_sse(&self, node_id: &str, tx: mpsc::Sender<SseEvent>) {
        if let Some(entry) = self.inner.write().await.by_id.get_mut(node_id) {
            entry.sse_tx = Some(tx);
        }
    }

    /// Detach a node's SSE sender, typically when the client disconnects.
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
        let mut inner = self.inner.write().await;
        if let Some(entry) = inner.by_id.remove(node_id) {
            inner.token_to_id.remove(&entry.token);
            true
        } else {
            false
        }
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
        let mut inner = self.inner.write().await;
        if let Some(entry) = inner.by_id.get_mut(node_id) {
            entry.last_heartbeat = Instant::now();
            entry.status = status;
            true
        } else {
            false
        }
    }

    /// Set the node's status without touching its heartbeat timestamp.
    ///
    /// Used by the dispatcher to flip `Idle → Busy` when handing off a task.
    pub async fn set_status(&self, node_id: &str, status: NodeStatus) -> bool {
        let mut inner = self.inner.write().await;
        if let Some(entry) = inner.by_id.get_mut(node_id) {
            entry.status = status;
            true
        } else {
            false
        }
    }

    /// Remove any node whose last heartbeat is older than `timeout`.
    ///
    /// Returns the list of reaped node_ids so callers can log them.
    pub async fn reap_stale(&self, timeout: Duration) -> Vec<NodeId> {
        let now = Instant::now();
        let mut reaped = Vec::new();
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

impl Default for NodeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Counts used by the `swarm_status` MCP tool.
#[derive(Debug, Default, Clone, Copy)]
pub struct RegistryCounts {
    pub total: usize,
    pub idle: usize,
    pub busy: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
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
            status: NodeStatus::Idle,
        }
    }

    #[tokio::test]
    async fn register_then_lookup() {
        let reg = NodeRegistry::new();
        let (node_id, token) = reg.register(test_manifest("alpha")).await;

        let looked_up = reg.node_id_for_token(&token).await;
        assert_eq!(looked_up.as_deref(), Some(node_id.as_str()));
    }

    #[tokio::test]
    async fn reap_stale_drops_expired_nodes() {
        let reg = NodeRegistry::new();
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
        let reg = NodeRegistry::new();
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
        let reg = NodeRegistry::new();
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
}
