//! The Dyson swarm hub — task routing server for Dyson nodes.
//!
//! This crate is both a binary (`swarm`) and a library.  The library form
//! exists so integration tests can spin up a hub in the same process as
//! a test harness without going through `main`.

pub mod auth;
pub mod blob;
pub mod config;
pub mod http;
pub mod key;
pub mod queue;
pub mod registry;
pub mod router;
pub mod tasks;
pub mod tls;

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::blob::BlobStore;
use crate::key::HubKeyPair;
use crate::registry::NodeRegistry;
use crate::tasks::persistence::{SqliteTaskPersistence, TaskPersistence};
use crate::tasks::{TaskStore, reconcile_orphaned_running};

/// Pre-validated static API key for MCP authentication.
///
/// Built once at startup from the `--mcp-api-key-hash` CLI arg.
/// The owner ID is derived from the hash so it stays stable across
/// requests without per-request allocation.
pub struct McpApiKey {
    /// The argon2id PHC string (e.g. `$argon2id$v=19$...`).
    pub hash: String,
    /// Stable synthetic owner ID (`apikey:<first-8-chars-of-hash-output>`).
    pub owner_id: String,
}

impl McpApiKey {
    /// Parse and validate a PHC hash string, pre-computing the owner ID.
    pub fn new(hash: String) -> Result<Self, argon2::password_hash::Error> {
        use argon2::password_hash::PasswordHash;
        PasswordHash::new(&hash)?;
        // Last $-segment is the hash output (base64, always ASCII).
        let tail = hash.rsplit('$').next().unwrap_or("apikey");
        let prefix = &tail[..tail.len().min(8)];
        let mut owner_id = String::with_capacity(7 + prefix.len());
        owner_id.push_str("apikey:");
        owner_id.push_str(prefix);
        Ok(Self { hash, owner_id })
    }

    /// Verify a plaintext token against the stored hash.
    pub fn verify(&self, token: &str) -> bool {
        use argon2::password_hash::PasswordHash;
        use argon2::{Argon2, PasswordVerifier};
        // Safe: hash was validated in new().
        let hash = PasswordHash::new(&self.hash).unwrap();
        Argon2::default()
            .verify_password(token.as_bytes(), &hash)
            .is_ok()
    }
}

/// A handle to the running hub shared across axum handlers.
///
/// Every HTTP handler takes `State<Arc<Hub>>` so it can reach the registry,
/// the blob store, and the signing key.
pub struct Hub {
    /// In-memory node registry.
    pub registry: NodeRegistry,
    /// Content-addressed blob store.
    pub blobs: BlobStore,
    /// The hub's signing key pair.  Used to sign dispatched tasks.
    pub key: HubKeyPair,
    /// Unified task state store.
    ///
    /// Holds state for every dispatched task — sync (blocking
    /// `swarm_dispatch`) and async (`swarm_submit`) alike.  Sync
    /// dispatches insert a `oneshot::Sender<SwarmResult>` as the
    /// `waiter` field on their record; async dispatches leave it `None`.
    /// `POST /swarm/result` drives both paths through
    /// `TaskStore::finalize`, guaranteeing one lock and one ordering.
    pub tasks: TaskStore,
    /// Static API key auth for the MCP endpoint.  When set, bearer
    /// tokens that don't match a registered node are verified against
    /// an argon2id hash as a fallback.
    pub mcp_api_key: Option<McpApiKey>,
    /// Broadcast channel used to tell long-lived handlers (specifically
    /// the SSE event stream) that the server is shutting down.
    ///
    /// Without this, axum's `with_graceful_shutdown` would wait forever
    /// for the open SSE connections to drain — they're indefinite by
    /// design — and Ctrl-C would appear to do nothing.
    shutdown: broadcast::Sender<()>,
}

impl Hub {
    /// Build a new hub from an already-loaded key and data directory.
    ///
    /// Opens the SQLite task persistence store at `data_dir/tasks.db`
    /// and rehydrates any previously-persisted tasks into memory.
    pub async fn new(
        key: HubKeyPair,
        data_dir: &std::path::Path,
        mcp_api_key: Option<McpApiKey>,
    ) -> std::io::Result<Arc<Self>> {
        let blobs = BlobStore::new(data_dir.join("blobs"))?;

        let db_path = data_dir.join("tasks.db");
        let persistence: Arc<dyn TaskPersistence> = Arc::new(
            SqliteTaskPersistence::open(&db_path)
                .await
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        );
        let mut recovered = persistence
            .load_all()
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let recovered_count = recovered.len();
        // Any task still marked Running after a restart is orphaned: its
        // node lost its SSE session and its node_id/token when the old
        // process died.  Flip them to Failed up-front so `swarm_task_*`
        // tools report a terminal state instead of lying.
        let orphaned_count = reconcile_orphaned_running(&*persistence, &mut recovered)
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let tasks = TaskStore::with_persistence(persistence, recovered);
        if recovered_count > 0 {
            tracing::info!(
                recovered = recovered_count,
                orphaned = orphaned_count,
                "recovered tasks from disk"
            );
        }

        // capacity = 1: we only ever broadcast once (on shutdown), and a
        // late subscriber will just see a "lagged" error that we ignore.
        let (shutdown, _) = broadcast::channel(1);
        Ok(Arc::new(Self {
            registry: NodeRegistry::new(),
            blobs,
            key,
            tasks,
            mcp_api_key,
            shutdown,
        }))
    }

    /// Subscribe to the shutdown signal.  The returned future resolves
    /// as soon as shutdown has been requested (or the broadcast sender is
    /// dropped, which also means we're shutting down).
    pub fn shutdown_notified(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut rx = self.shutdown.subscribe();
        async move {
            let _ = rx.recv().await;
        }
    }

    /// Request a graceful shutdown.  Wakes every `shutdown_notified()`
    /// future so SSE streams end and the registry reaper can exit.
    pub fn trigger_shutdown(&self) {
        // Send failure just means nobody was listening — fine, we still
        // want shutdown semantics (the main task owns the only sender).
        let _ = self.shutdown.send(());
    }
}
