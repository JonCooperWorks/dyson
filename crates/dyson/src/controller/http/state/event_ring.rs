//! Per-chat SSE replay ring.

use super::SseEvent;

/// Per-chat replay ring buffer.  Capacity is small and fixed so the
/// memory bound is obvious — covers a normal reconnect window.
pub(crate) struct EventRing {
    pub(crate) entries: std::collections::VecDeque<(u64, SseEvent)>,
    /// Monotonic counter; the next event minted gets `next_id`.
    pub(crate) next_id: u64,
}

impl EventRing {
    /// Maximum entries kept.  Matched to the broadcast channel
    /// capacity (`broadcast::channel(4096)` in `ChatHandle::new`) so
    /// a subscriber that hits `RecvError::Lagged` can still resume
    /// from the ring — with the previous 256-entry ring, events
    /// 257..4096 behind the cursor were dropped from the channel
    /// AND already evicted from the ring, leaving a permanent gap
    /// for that one client.  `SseEvent` payloads are small (text
    /// deltas, tool refs); 4096 of them is a few MB per active chat
    /// at worst, fine for the deployment scale.
    pub(super) const CAP: usize = 4096;

    pub(crate) fn new() -> Self {
        Self {
            // Grow on demand: most chats never get near CAP, and an
            // idle handle should not pin a full ring's worth of slots.
            entries: std::collections::VecDeque::new(),
            next_id: 1,
        }
    }

    /// Push an event onto the ring and return the id it was tagged
    /// with.  Wraps oldest-first when at capacity.
    pub(crate) fn push(&mut self, evt: SseEvent) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        if self.entries.len() >= Self::CAP {
            self.entries.pop_front();
        }
        self.entries.push_back((id, evt));
        id
    }

    /// Snapshot of every entry with id > `since`.  Used by SSE
    /// reconnect to replay only what the client missed.  Cloned
    /// because the caller holds the mutex briefly and then iterates.
    pub(crate) fn since(&self, since: u64) -> Vec<(u64, SseEvent)> {
        // Ids are pushed in increasing order, so binary-search the cut
        // instead of scanning every entry.
        let start = self.entries.partition_point(|(id, _)| *id <= since);
        self.entries.range(start..).cloned().collect()
    }

    /// Drop every entry but keep the monotonic counter.  Called at
    /// end-of-turn so a fresh `EventSource` opening for the *next*
    /// turn (no `Last-Event-ID`, server treats `since=0`) doesn't
    /// replay the previous turn's text deltas into the new agent
    /// placeholder — the visible "subsequent message responds with
    /// the last message" duplication bug.  Keeping `next_id`
    /// monotonic protects any live subscriber from observing a
    /// regressed id on the next emit.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}
