//! Per-peer mailbox with TTL queueing.
//!
//! When a destination peer is connected, envelopes flow straight through
//! to its inbox channel. When the peer is disconnected, envelopes queue
//! up here with an expiration timestamp; reconnect drains the queue
//! (minus anything that timed out).
//!
//! The mailbox is intentionally ephemeral. It does not persist across
//! relay restarts. Services that need durability handle it themselves
//! (e.g. the scheduler's SQLite task table).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::envelope::MeshEnvelope;

/// One queued envelope plus an expiration timestamp.
#[derive(Debug)]
pub struct QueuedEnvelope {
    pub envelope: MeshEnvelope,
    pub expires_at: Instant,
}

impl QueuedEnvelope {
    pub fn new(envelope: MeshEnvelope) -> Self {
        let ttl = Duration::from_secs(envelope.ttl_secs);
        Self {
            envelope,
            expires_at: Instant::now() + ttl,
        }
    }

    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.expires_at
    }
}

/// A bounded FIFO queue of envelopes for one disconnected peer.
#[derive(Debug, Default)]
pub struct Mailbox {
    queue: VecDeque<QueuedEnvelope>,
}

impl Mailbox {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    pub fn push(&mut self, envelope: MeshEnvelope) {
        self.queue.push_back(QueuedEnvelope::new(envelope));
    }

    /// Drop expired envelopes. Returns the count dropped.
    pub fn sweep_expired(&mut self) -> usize {
        let now = Instant::now();
        let before = self.queue.len();
        self.queue.retain(|q| !q.is_expired(now));
        before - self.queue.len()
    }

    /// Drain everything that hasn't expired, in FIFO order.
    pub fn drain_live(&mut self) -> Vec<MeshEnvelope> {
        let now = Instant::now();
        let mut out = Vec::new();
        while let Some(q) = self.queue.pop_front() {
            if !q.is_expired(now) {
                out.push(q.envelope);
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::{MeshAddr, ServiceName};
    use crate::envelope::{MessageKind, MeshEnvelope};
    use crate::identity::NodeIdentity;

    fn make_envelope(ttl: Duration) -> MeshEnvelope {
        let alice = NodeIdentity::generate_ephemeral();
        let bob = NodeIdentity::generate_ephemeral();
        MeshEnvelope::new_with_ttl(
            MeshAddr::new(alice.node_id().clone(), ServiceName::from("a")),
            MeshAddr::new(bob.node_id().clone(), ServiceName::from("b")),
            MessageKind::TaskProgress,
            serde_json::json!({}),
            ttl,
        )
    }

    #[test]
    fn push_then_drain() {
        let mut mb = Mailbox::new();
        mb.push(make_envelope(Duration::from_secs(60)));
        mb.push(make_envelope(Duration::from_secs(60)));
        assert_eq!(mb.len(), 2);
        let drained = mb.drain_live();
        assert_eq!(drained.len(), 2);
        assert!(mb.is_empty());
    }

    #[test]
    fn sweep_drops_expired() {
        let mut mb = Mailbox::new();
        // Manually backdate the expires_at so we don't have to actually sleep.
        mb.push(make_envelope(Duration::from_secs(60)));
        if let Some(q) = mb.queue.front_mut() {
            q.expires_at = Instant::now() - Duration::from_secs(1);
        }
        let dropped = mb.sweep_expired();
        assert_eq!(dropped, 1);
        assert!(mb.is_empty());
    }

    #[test]
    fn drain_live_skips_expired() {
        let mut mb = Mailbox::new();
        mb.push(make_envelope(Duration::from_secs(60)));
        mb.push(make_envelope(Duration::from_secs(60)));
        if let Some(q) = mb.queue.front_mut() {
            q.expires_at = Instant::now() - Duration::from_secs(1);
        }
        let live = mb.drain_live();
        assert_eq!(live.len(), 1);
    }
}
