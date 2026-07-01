// ===========================================================================
// CoalescingPersister — off-runtime, latest-wins transcript checkpointing.
//
// The HTTP controller checkpoints the transcript after every message push
// (crash safety for long turns).  Doing that inline re-serialized the
// entire history synchronously on the async runtime for every push —
// O(n²) work per turn that grew with conversation length and stalled the
// event loop.
//
// This type decouples the hook from the disk:
//   - `schedule()` stores the newest snapshot (latest-wins: rapid
//     successive persists coalesce — only the most recent snapshot is
//     ever written next) and wakes a single background worker.
//   - The worker debounces briefly, then runs `ChatHistory::save` on the
//     blocking pool via `spawn_blocking`.
//   - `checkpoint()` schedules a snapshot and awaits completion, giving
//     controller safe-points a hard guarantee before `Done` is emitted
//     or cancellation releases the turn.
//
// Lifecycle: the worker task exits when the last clone of the persister
// is dropped (the persist hook installed on the cached agent holds one,
// so a chat's worker lives until the hook is replaced on the next turn
// or the agent is evicted).
// ===========================================================================

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::chat_history::ChatHistory;
use crate::message::Message;

/// Short debounce so a burst of pushes (tool batch finishing, several
/// messages admitted at once) collapses into one write.  Small enough
/// that the crash-safety window stays negligible next to turn length.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(100);

struct Inner {
    history: Arc<dyn ChatHistory>,
    chat_id: String,
    /// Newest pending snapshot with its generation number.  `schedule`
    /// overwrites (latest-wins); the worker takes it.
    latest: std::sync::Mutex<Option<(u64, Vec<Message>)>>,
    /// Generation of the most recently scheduled snapshot.
    scheduled: AtomicU64,
    /// Broadcasts the generation of the most recently completed write.
    completed_tx: tokio::sync::watch::Sender<u64>,
    /// Wakes the worker.  Held separately by the worker task so a
    /// dropped `Inner` can still deliver the shutdown wake-up.
    notify: Arc<tokio::sync::Notify>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Wake the worker so it observes the dead Weak and exits.
        self.notify.notify_one();
    }
}

/// Cloneable handle: one clone lives in the agent's persist hook, one in
/// the turn task for the end-of-turn `flush()`.
#[derive(Clone)]
pub(crate) struct CoalescingPersister {
    inner: Arc<Inner>,
}

impl CoalescingPersister {
    pub(crate) fn new(history: Arc<dyn ChatHistory>, chat_id: String) -> Self {
        let notify = Arc::new(tokio::sync::Notify::new());
        let (completed_tx, _) = tokio::sync::watch::channel(0u64);
        let inner = Arc::new(Inner {
            history,
            chat_id,
            latest: std::sync::Mutex::new(None),
            scheduled: AtomicU64::new(0),
            completed_tx,
            notify: Arc::clone(&notify),
        });

        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            loop {
                notify.notified().await;
                loop {
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    // Debounce: give a burst of schedules a beat to
                    // coalesce before paying for a serialize+write.
                    tokio::time::sleep(DEBOUNCE).await;
                    let taken = {
                        let mut guard = inner
                            .latest
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        guard.take()
                    };
                    let Some((generation, messages)) = taken else {
                        break; // drained — park until the next schedule
                    };
                    let history = Arc::clone(&inner.history);
                    let chat_id = inner.chat_id.clone();
                    let result =
                        tokio::task::spawn_blocking(move || history.save(&chat_id, &messages))
                            .await;
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            tracing::warn!(
                                error = %e,
                                chat_id = %inner.chat_id,
                                "background transcript checkpoint failed"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                chat_id = %inner.chat_id,
                                "background transcript checkpoint task panicked"
                            );
                        }
                    }
                    // Mark progress even on failure so flush() can't hang
                    // on a persistently-broken disk.
                    let _ = inner.completed_tx.send(generation);
                }
            }
        });

        Self { inner }
    }

    /// Record the newest transcript snapshot for writing.  Cheap,
    /// non-blocking, and best-effort until a later [`Self::flush`] or
    /// [`Self::checkpoint`] reaches disk; rapid calls coalesce (latest wins).
    pub(crate) fn schedule(&self, messages: Vec<Message>) {
        let generation = self.inner.scheduled.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut guard = self
                .inner
                .latest
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some((generation, messages));
        }
        self.inner.notify.notify_one();
    }

    /// Schedule a snapshot and wait until it has either reached the
    /// backing store or definitively failed.  Controllers use this at
    /// cancellation/end-of-turn boundaries where the transcript must be
    /// durable before user-visible completion state is emitted.
    pub(crate) async fn checkpoint(&self, messages: Vec<Message>) {
        self.schedule(messages);
        self.flush().await;
    }

    /// Wait until every snapshot scheduled before this call has been
    /// written (or has definitively failed).  Called at end of turn so
    /// the transcript on disk is current before `Done` is emitted.
    pub(crate) async fn flush(&self) {
        let target = self.inner.scheduled.load(Ordering::SeqCst);
        let mut rx = self.inner.completed_tx.subscribe();
        while *rx.borrow_and_update() < target {
            if rx.changed().await.is_err() {
                return; // worker gone — nothing more will complete
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use std::sync::Mutex;

    /// Recording backend: counts saves, remembers the last snapshot, and
    /// optionally sleeps to simulate slow disk (forcing coalescing).
    struct SlowRecordingHistory {
        saves: Mutex<Vec<Vec<Message>>>,
        delay: std::time::Duration,
    }

    impl ChatHistory for SlowRecordingHistory {
        fn save(&self, _chat_id: &str, messages: &[Message]) -> Result<()> {
            std::thread::sleep(self.delay);
            self.saves.lock().unwrap().push(messages.to_vec());
            Ok(())
        }
        fn load(&self, _chat_id: &str) -> Result<Vec<Message>> {
            Ok(Vec::new())
        }
        fn rotate(&self, _chat_id: &str) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rapid_schedules_coalesce_to_latest_wins() {
        let history = Arc::new(SlowRecordingHistory {
            saves: Mutex::new(Vec::new()),
            delay: std::time::Duration::from_millis(30),
        });
        let persister = CoalescingPersister::new(
            Arc::clone(&history) as Arc<dyn ChatHistory>,
            "c-test".into(),
        );

        // 20 rapid pushes — far more than the disk could keep up with
        // synchronously.
        for i in 0..20 {
            let mut messages = Vec::new();
            for j in 0..=i {
                messages.push(Message::user(&format!("msg {j}")));
            }
            persister.schedule(messages);
        }
        persister.flush().await;

        let saves = history.saves.lock().unwrap();
        assert!(
            !saves.is_empty(),
            "at least one write must have happened by flush()"
        );
        assert!(
            saves.len() < 20,
            "rapid schedules must coalesce, got {} writes for 20 schedules",
            saves.len()
        );
        // Final flush guarantees the LAST snapshot is on disk.
        let last = saves.last().unwrap();
        assert_eq!(last.len(), 20, "the newest snapshot must win");
        assert!(matches!(
            &last[19].content[0],
            crate::message::ContentBlock::Text { text } if text == "msg 19"
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_guarantees_write_even_for_a_single_schedule() {
        let history = Arc::new(SlowRecordingHistory {
            saves: Mutex::new(Vec::new()),
            delay: std::time::Duration::ZERO,
        });
        let persister = CoalescingPersister::new(
            Arc::clone(&history) as Arc<dyn ChatHistory>,
            "c-test".into(),
        );
        persister.schedule(vec![Message::user("only one")]);
        persister.flush().await;
        assert_eq!(history.saves.lock().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_with_nothing_scheduled_returns_immediately() {
        let history = Arc::new(SlowRecordingHistory {
            saves: Mutex::new(Vec::new()),
            delay: std::time::Duration::ZERO,
        });
        let persister = CoalescingPersister::new(
            Arc::clone(&history) as Arc<dyn ChatHistory>,
            "c-test".into(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), persister.flush())
            .await
            .expect("flush with an empty queue must not block");
        assert!(history.saves.lock().unwrap().is_empty());
    }
}
