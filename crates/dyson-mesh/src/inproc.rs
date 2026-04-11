//! In-process [`MeshClient`] implementation backed by tokio channels.
//!
//! This impl exists for two reasons:
//!
//! 1. **Tests.** A `Vec<Arc<dyn MeshClient>>` of in-process peers spun up
//!    in a single test process can exercise the full scheduler /
//!    notifier / cancellation flow without HTTP, SQLite, or sleeps.
//!
//! 2. **Hub-local short-circuit.** When the hub binary hosts the
//!    scheduler, notifier, and MCP services in the same process as the
//!    relay, those services use an `InProcMeshClient` so internal sends
//!    skip serialization and the network entirely. The trait surface is
//!    identical to the HTTP impl — the optimization is invisible to
//!    callers.
//!
//! This module also exposes [`InProcRelay`], the shared state that
//! every `InProcMeshClient` connects to. A test creates one relay and
//! spawns multiple clients against it; in production the hub binary owns
//! exactly one.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::Stream;
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::{BroadcastStream, ReceiverStream};

use crate::addr::{NodeId, ServiceName};
use crate::envelope::MeshEnvelope;
use crate::error::{MeshError, Result};
use crate::mailbox::Mailbox;
use crate::mesh::{BoxStream, MeshClient, PeerEvent, PeerInfo, ServiceDescriptor};

/// Shared state for the in-process relay.
///
/// One [`InProcRelay`] supports many [`InProcMeshClient`]s, each
/// representing one peer in the swarm.
#[derive(Clone)]
pub struct InProcRelay {
    inner: Arc<RwLock<RelayInner>>,
    peer_events_tx: broadcast::Sender<PeerEvent>,
}

struct RelayInner {
    peers: HashMap<NodeId, PeerSlot>,
}

struct PeerSlot {
    info: PeerInfo,
    /// Per-service inboxes. Each connected service has an mpsc sender.
    services: HashMap<ServiceName, mpsc::Sender<MeshEnvelope>>,
    /// Mailbox for envelopes destined to a service that has no live inbox.
    mailbox: Mailbox,
}

impl InProcRelay {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(RwLock::new(RelayInner {
                peers: HashMap::new(),
            })),
            peer_events_tx: tx,
        }
    }

    /// Snapshot of the peer table.
    pub async fn peers(&self) -> Vec<PeerInfo> {
        self.inner
            .read()
            .await
            .peers
            .values()
            .map(|s| s.info.clone())
            .collect()
    }

    async fn upsert_peer(&self, info: PeerInfo) {
        let mut inner = self.inner.write().await;
        let entry = inner
            .peers
            .entry(info.node_id.clone())
            .or_insert_with(|| PeerSlot {
                info: info.clone(),
                services: HashMap::new(),
                mailbox: Mailbox::new(),
            });
        let updated = entry.info.services != info.services;
        entry.info = info.clone();
        let event = if updated {
            PeerEvent::ManifestUpdated(info)
        } else {
            PeerEvent::Joined(info)
        };
        let _ = self.peer_events_tx.send(event);
    }

    async fn remove_peer(&self, node: &NodeId) {
        let mut inner = self.inner.write().await;
        if inner.peers.remove(node).is_some() {
            let _ = self.peer_events_tx.send(PeerEvent::Departed(node.clone()));
        }
    }

    /// Attach a service inbox for a peer. Returns a receiver the caller
    /// reads from. Calling this drains any queued envelopes for that
    /// service first.
    async fn attach_service(
        &self,
        node: &NodeId,
        service: &ServiceName,
    ) -> mpsc::Receiver<MeshEnvelope> {
        let (tx, rx) = mpsc::channel::<MeshEnvelope>(64);
        let mut inner = self.inner.write().await;
        let slot = inner
            .peers
            .entry(node.clone())
            .or_insert_with(|| PeerSlot {
                info: PeerInfo {
                    node_id: node.clone(),
                    display_name: String::new(),
                    services: vec![],
                    manifest: serde_json::Value::Null,
                },
                services: HashMap::new(),
                mailbox: Mailbox::new(),
            });
        slot.services.insert(service.clone(), tx.clone());

        // Drain any queued envelopes destined for this service.
        let queued = slot.mailbox.drain_live();
        drop(inner);
        for env in queued {
            if env.to.service == *service {
                let _ = tx.send(env).await;
            } else {
                // Re-queue envelopes for other services. We took the lock
                // above so we drained everything, but we only want to
                // deliver this service's envelopes here.
                let _ = self.deliver(env).await;
            }
        }

        rx
    }

    async fn detach_service(&self, node: &NodeId, service: &ServiceName) {
        let mut inner = self.inner.write().await;
        if let Some(slot) = inner.peers.get_mut(node) {
            slot.services.remove(service);
        }
    }

    /// Deliver an envelope. If the destination service has a live inbox
    /// it goes straight there; otherwise it's queued in the destination
    /// peer's mailbox until the service attaches (or the TTL expires).
    pub async fn deliver(&self, envelope: MeshEnvelope) -> Result<()> {
        if envelope.is_expired() {
            return Err(MeshError::Expired);
        }

        let target_node = envelope.to.node.clone();
        let target_service = envelope.to.service.clone();

        // First try a fast path: read lock + clone the sender.
        let live_tx = {
            let inner = self.inner.read().await;
            inner
                .peers
                .get(&target_node)
                .and_then(|slot| slot.services.get(&target_service).cloned())
        };

        if let Some(tx) = live_tx {
            tx.send(envelope)
                .await
                .map_err(|_| MeshError::PeerDisconnected(target_node.to_string()))?;
            return Ok(());
        }

        // Slow path: queue in the mailbox.
        let mut inner = self.inner.write().await;
        let slot = inner
            .peers
            .entry(target_node.clone())
            .or_insert_with(|| PeerSlot {
                info: PeerInfo {
                    node_id: target_node.clone(),
                    display_name: String::new(),
                    services: vec![],
                    manifest: serde_json::Value::Null,
                },
                services: HashMap::new(),
                mailbox: Mailbox::new(),
            });
        slot.mailbox.sweep_expired();
        slot.mailbox.push(envelope);
        Ok(())
    }

    pub fn subscribe_peer_events(&self) -> broadcast::Receiver<PeerEvent> {
        self.peer_events_tx.subscribe()
    }
}

impl Default for InProcRelay {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// InProcMeshClient
// ---------------------------------------------------------------------------

/// A [`MeshClient`] backed by an [`InProcRelay`].
#[derive(Clone)]
pub struct InProcMeshClient {
    relay: InProcRelay,
    local: NodeId,
}

impl InProcMeshClient {
    pub fn new(relay: InProcRelay, local: NodeId) -> Self {
        Self { relay, local }
    }

    /// Direct access to the relay (used for testing and for hub-local
    /// services that need to peek at peer state).
    pub fn relay(&self) -> &InProcRelay {
        &self.relay
    }
}

#[async_trait]
impl MeshClient for InProcMeshClient {
    fn local_node(&self) -> &NodeId {
        &self.local
    }

    async fn announce(
        &self,
        display_name: &str,
        services: Vec<ServiceDescriptor>,
        manifest: serde_json::Value,
    ) -> Result<()> {
        let info = PeerInfo {
            node_id: self.local.clone(),
            display_name: display_name.to_string(),
            services,
            manifest,
        };
        self.relay.upsert_peer(info).await;
        Ok(())
    }

    async fn depart(&self) -> Result<()> {
        self.relay.remove_peer(&self.local).await;
        Ok(())
    }

    async fn peers(&self) -> Result<Vec<PeerInfo>> {
        Ok(self.relay.peers().await)
    }

    fn peer_events(&self) -> BoxStream<'static, PeerEvent> {
        let rx = self.relay.subscribe_peer_events();
        // Drop lagged errors so consumers don't have to care.
        let stream = BroadcastStream::new(rx).filter_map(|res| res.ok());
        Pin::from(Box::new(stream) as Box<dyn Stream<Item = PeerEvent> + Send>)
    }

    async fn send(&self, envelope: MeshEnvelope) -> Result<()> {
        self.relay.deliver(envelope).await
    }

    fn inbox(&self, service: &ServiceName) -> BoxStream<'static, MeshEnvelope> {
        let relay = self.relay.clone();
        let local = self.local.clone();
        let service = service.clone();
        // Use an async block to attach lazily and yield a stream.
        let (tx, rx) = mpsc::channel::<MeshEnvelope>(64);
        tokio::spawn(async move {
            let mut inbox_rx = relay.attach_service(&local, &service).await;
            while let Some(env) = inbox_rx.recv().await {
                if tx.send(env).await.is_err() {
                    break;
                }
            }
            relay.detach_service(&local, &service).await;
        });
        Pin::from(Box::new(ReceiverStream::new(rx)) as Box<dyn Stream<Item = MeshEnvelope> + Send>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::MeshAddr;
    use crate::envelope::{MessageKind, MeshEnvelope};
    use crate::identity::NodeIdentity;
    use std::time::Duration;

    fn addr(id: &NodeIdentity, service: &str) -> MeshAddr {
        MeshAddr::new(id.node_id().clone(), ServiceName::from(service))
    }

    #[tokio::test]
    async fn round_trip_two_peers() {
        let relay = InProcRelay::new();
        let alice = NodeIdentity::generate_ephemeral();
        let bob = NodeIdentity::generate_ephemeral();

        let client_a = InProcMeshClient::new(relay.clone(), alice.node_id().clone());
        let client_b = InProcMeshClient::new(relay.clone(), bob.node_id().clone());

        client_a
            .announce("alice", vec![], serde_json::json!({}))
            .await
            .unwrap();
        client_b
            .announce("bob", vec![], serde_json::json!({}))
            .await
            .unwrap();

        let mut bob_inbox = client_b.inbox(&ServiceName::from("scheduler"));

        // Give the spawned attach task a moment to wire up.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut env = MeshEnvelope::new(
            addr(&alice, "agent"),
            addr(&bob, "scheduler"),
            MessageKind::SubmitTask,
            serde_json::json!({"prompt": "hello"}),
        );
        env.sign(&alice).unwrap();
        client_a.send(env.clone()).await.unwrap();

        let received = tokio::time::timeout(Duration::from_secs(1), bob_inbox.next())
            .await
            .unwrap()
            .expect("inbox closed");
        assert_eq!(received.request_id, env.request_id);
        received.verify().unwrap();
    }

    #[tokio::test]
    async fn mailbox_queues_until_attach() {
        let relay = InProcRelay::new();
        let alice = NodeIdentity::generate_ephemeral();
        let bob = NodeIdentity::generate_ephemeral();
        let client_a = InProcMeshClient::new(relay.clone(), alice.node_id().clone());
        let client_b = InProcMeshClient::new(relay.clone(), bob.node_id().clone());

        // Send before bob attaches.
        let mut env = MeshEnvelope::new(
            addr(&alice, "agent"),
            addr(&bob, "scheduler"),
            MessageKind::SubmitTask,
            serde_json::json!({"prompt": "queued"}),
        );
        env.sign(&alice).unwrap();
        client_a.send(env.clone()).await.unwrap();

        // Now bob attaches.
        let mut bob_inbox = client_b.inbox(&ServiceName::from("scheduler"));
        let received = tokio::time::timeout(Duration::from_secs(1), bob_inbox.next())
            .await
            .unwrap()
            .expect("inbox closed");
        assert_eq!(received.request_id, env.request_id);
    }

    #[tokio::test]
    async fn announce_visible_in_peers() {
        let relay = InProcRelay::new();
        let alice = NodeIdentity::generate_ephemeral();
        let client = InProcMeshClient::new(relay.clone(), alice.node_id().clone());
        client
            .announce(
                "alice",
                vec![ServiceDescriptor {
                    name: ServiceName::from("scheduler"),
                    version: "1".into(),
                }],
                serde_json::json!({"role": "hub"}),
            )
            .await
            .unwrap();
        let peers = client.peers().await.unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].node_id, *alice.node_id());
        assert_eq!(peers[0].services.len(), 1);
    }

    #[tokio::test]
    async fn depart_removes_peer() {
        let relay = InProcRelay::new();
        let alice = NodeIdentity::generate_ephemeral();
        let client = InProcMeshClient::new(relay.clone(), alice.node_id().clone());
        client
            .announce("alice", vec![], serde_json::json!({}))
            .await
            .unwrap();
        client.depart().await.unwrap();
        let peers = client.peers().await.unwrap();
        assert!(peers.is_empty());
    }
}
