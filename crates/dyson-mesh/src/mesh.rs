//! The [`MeshClient`] trait — the contract every transport implements.
//!
//! Today there are two impls:
//!
//! - [`crate::InProcMeshClient`] — in-memory channels, used by tests and
//!   by hub-local services that share a process with the relay
//! - `HttpMeshClient` (lives in the worker) — talks to a remote relay
//!   over HTTP
//!
//! A future `GossipMeshClient` impl can drop in behind the same trait
//! without changing the scheduler, notifier, MCP service, or worker code.

use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::Stream;
use serde::{Deserialize, Serialize};

use crate::addr::{NodeId, ServiceName};
use crate::envelope::MeshEnvelope;
use crate::error::Result;

/// A short description of a service hosted on a peer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceDescriptor {
    pub name: ServiceName,
    /// Free-form version string. Services use it however they like.
    #[serde(default)]
    pub version: String,
}

/// What the relay knows about a peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub node_id: NodeId,
    /// Optional human label (hostname, role, etc.).
    #[serde(default)]
    pub display_name: String,
    /// Services this peer hosts.
    #[serde(default)]
    pub services: Vec<ServiceDescriptor>,
    /// Opaque manifest details, JSON-encoded for forward-compat. The
    /// scheduler treats this as a hint for routing.
    #[serde(default)]
    pub manifest: serde_json::Value,
}

/// Lifecycle event emitted by [`MeshClient::peer_events`].
#[derive(Debug, Clone)]
pub enum PeerEvent {
    Joined(PeerInfo),
    Departed(NodeId),
    ManifestUpdated(PeerInfo),
}

/// Boxed stream type used by trait methods.
pub type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;

/// The mesh client trait.
///
/// Implementations are responsible for transport, framing, and per-peer
/// reconnect / mailbox management. Consumers (the scheduler, notifier,
/// worker, MCP service) only see this trait.
#[async_trait]
pub trait MeshClient: Send + Sync + 'static {
    /// This peer's node id.
    fn local_node(&self) -> &NodeId;

    /// Publish (or refresh) the local peer's manifest. Calling this
    /// repeatedly is the way services advertise themselves to the rest
    /// of the swarm.
    async fn announce(
        &self,
        display_name: &str,
        services: Vec<ServiceDescriptor>,
        manifest: serde_json::Value,
    ) -> Result<()>;

    /// Leave the mesh cleanly.
    async fn depart(&self) -> Result<()>;

    /// Snapshot of currently reachable peers.
    async fn peers(&self) -> Result<Vec<PeerInfo>>;

    /// Stream of peer lifecycle events. Each call returns a fresh stream;
    /// implementations may multiplex internally.
    fn peer_events(&self) -> BoxStream<'static, PeerEvent>;

    /// Send a fully-formed envelope. The implementation will fill in or
    /// validate `from`. Caller is responsible for signing.
    async fn send(&self, envelope: MeshEnvelope) -> Result<()>;

    /// Inbox for a specific service hosted by *this* peer. Each call
    /// returns an independent stream filtered to envelopes whose
    /// `to.service` matches.
    fn inbox(&self, service: &ServiceName) -> BoxStream<'static, MeshEnvelope>;
}

/// Default mailbox TTL when callers don't supply one.
pub const DEFAULT_MAILBOX_TTL: Duration = Duration::from_secs(600);
