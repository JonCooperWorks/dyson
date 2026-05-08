use std::sync::Arc;

use crate::chat_history::ChatHistory;

use super::{Agent, HistoryBackend, PersistHook};

impl Agent {
    /// Install a callback that runs after every message push.  Used by the
    /// HTTP controller to checkpoint the transcript to disk mid-turn.
    pub fn set_persist_hook(&mut self, hook: PersistHook) {
        self.persist_hook = Some(hook);
    }

    /// Fire the persist hook with the current transcript.  Cheap and
    /// idempotent when no hook is installed; controllers decide whether
    /// to actually hit disk.
    pub(crate) fn persist(&self) {
        if let Some(hook) = &self.persist_hook {
            hook(&self.conversation.messages);
        }
    }

    /// Attach a chat history backend so compaction can rotate pre-compaction
    /// snapshots for fine-tuning.
    ///
    /// When set, every compaction will first save the current conversation
    /// to a timestamped archive file (via `ChatHistory::rotate`) before
    /// summarising.  This preserves the full verbatim history.
    pub fn set_chat_history(&mut self, store: Arc<dyn ChatHistory>, chat_id: String) {
        self.history_backend = Some(HistoryBackend { store, chat_id });
    }
}
