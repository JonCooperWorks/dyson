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
use crate::tasks::TaskStore;

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
    pub fn new(key: HubKeyPair, data_dir: &std::path::Path) -> std::io::Result<Arc<Self>> {
        let blobs = BlobStore::new(data_dir.join("blobs"))?;
        // capacity = 1: we only ever broadcast once (on shutdown), and a
        // late subscriber will just see a "lagged" error that we ignore.
        let (shutdown, _) = broadcast::channel(1);
        Ok(Arc::new(Self {
            registry: NodeRegistry::new(),
            blobs,
            key,
            tasks: TaskStore::new(),
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
