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

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::blob::BlobStore;
use crate::key::HubKeyPair;
use crate::registry::NodeRegistry;

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
    /// Pending dispatches awaiting results.
    ///
    /// When the MCP endpoint signs and dispatches a task, it inserts a
    /// oneshot sender keyed on the task_id.  When `POST /swarm/result`
    /// arrives, the result is pushed through that sender and the MCP
    /// caller wakes up.
    pub pending_dispatches: Mutex<
        std::collections::HashMap<
            String,
            tokio::sync::oneshot::Sender<dyson_swarm_protocol::types::SwarmResult>,
        >,
    >,
}

impl Hub {
    /// Build a new hub from an already-loaded key and data directory.
    pub fn new(key: HubKeyPair, data_dir: &std::path::Path) -> std::io::Result<Arc<Self>> {
        let blobs = BlobStore::new(data_dir.join("blobs"))?;
        Ok(Arc::new(Self {
            registry: NodeRegistry::new(),
            blobs,
            key,
            pending_dispatches: Mutex::new(std::collections::HashMap::new()),
        }))
    }
}
