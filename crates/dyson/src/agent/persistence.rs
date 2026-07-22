use std::sync::Arc;

use crate::chat_history::ChatHistory;

use super::protocol::{RunEvent, RunEventKind, RunId, RunStatus};
use super::{Agent, HistoryBackend, PersistHook};

impl Agent {
    pub(crate) fn emit_run_event(&self, kind: RunEventKind) {
        // Sequence assignment and durable append share one critical section.
        // Parallel tool futures otherwise could allocate N/N+1 and append them
        // in reverse order, making the canonical replay stream non-monotonic.
        let mut sequence = self
            .event_sequence
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *sequence += 1;
        let event = RunEvent::new(
            *sequence,
            self.active_run_id.clone(),
            self.conversation.turn_count,
            kind,
        );
        if let Some(backend) = &self.history_backend
            && let Err(error) = backend.store.append_run_event(&backend.chat_id, &event)
        {
            tracing::error!(error = %error, "failed to persist canonical run event");
        }
    }

    pub(crate) fn begin_run_protocol(&mut self) {
        self.active_run_id = RunId::new();
        *self
            .event_sequence
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = 0;
        self.last_run_status = RunStatus::Completed;
        self.emit_run_event(RunEventKind::RunStarted);
    }

    pub(crate) fn finish_run_protocol<T>(&mut self, result: &crate::error::Result<T>) {
        self.last_run_status = if self.tool_context.cancellation.is_cancelled() {
            RunStatus::Cancelled
        } else if result.is_err() {
            RunStatus::Failed
        } else {
            self.last_run_status
        };
        self.emit_run_event(RunEventKind::RunFinished {
            status: self.last_run_status,
        });
    }

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

    /// Replay the attached journal and return side effects that were in flight
    /// when the prior process stopped. Callers can reconcile these explicitly;
    /// the agent never retries an unknown outcome automatically.
    pub fn unresolved_tool_outcomes(
        &self,
    ) -> crate::error::Result<Vec<super::protocol::UnresolvedToolOutcome>> {
        let Some(backend) = &self.history_backend else {
            return Ok(Vec::new());
        };
        let events = backend.store.load_run_events(&backend.chat_id)?;
        Ok(super::protocol::unresolved_tool_outcomes(&events))
    }
}
