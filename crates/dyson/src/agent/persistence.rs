use std::sync::Arc;

use crate::chat_history::ChatHistory;

use super::protocol::{RunEvent, RunEventKind, RunId, RunStatus};
use super::{Agent, HistoryBackend, PersistHook};

impl Agent {
    pub(crate) fn try_emit_run_event(&self, kind: RunEventKind) -> crate::error::Result<()> {
        let mut sequence = self
            .event_sequence
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *sequence += 1;
        let event = RunEvent::new(
            *sequence,
            self.active_run_id.clone(),
            self.conversation.turn_count,
            kind,
        );
        if let Some(backend) = &self.history_backend {
            backend.store.append_run_event(&backend.chat_id, &event)?;
        }
        Ok(())
    }

    pub(crate) fn emit_run_event(&self, kind: RunEventKind) {
        if let Err(error) = self.try_emit_run_event(kind) {
            self.run_warnings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("Journal write failed: {error}"));
        }
    }

    pub(crate) fn begin_run_protocol(&mut self) -> crate::error::Result<()> {
        self.begin_run_protocol_with_mode(false)
    }

    pub(super) fn begin_run_protocol_with_mode(
        &mut self,
        direct: bool,
    ) -> crate::error::Result<()> {
        self.tool_context.harness.ensure_loaded()?;
        if let Some(input) = self
            .conversation
            .messages
            .iter()
            .rev()
            .find(|m| m.role == crate::message::Role::User)
            .and_then(crate::message::Message::last_text)
        {
            self.tool_context.harness.seed_objective(input);
        }
        self.tool_context
            .harness
            .budget
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .started_ms
            .get_or_insert_with(super::task::budget::now_ms);
        self.active_run_id = RunId::new();
        *self
            .event_sequence
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = 0;
        self.run_warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.limiter.reset_turn();
        self.repeated_failures.clear();
        self.repeated_observations.clear();
        self.last_run_status = RunStatus::Partial;
        self.try_emit_run_event(RunEventKind::RunStarted)?;
        self.begin_continuation(direct)?;
        let unresolved = self.unresolved_tool_outcomes()?;
        if !unresolved.is_empty() {
            let warning = format!(
                "{} prior tool outcome(s) require reconciliation. Read-only investigation is allowed; mutations are withheld. Unresolved calls: {}",
                unresolved.len(),
                serde_json::to_string(&unresolved)?
            );
            self.run_warnings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(warning.clone());
            self.conversation
                .messages
                .push(crate::message::Message::user(&warning));
        }
        Ok(())
    }

    pub(crate) fn finish_run_protocol<T>(&mut self, result: &crate::error::Result<T>) {
        if let Err(error) = self.tool_context.harness.checkpoint() {
            self.run_warnings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(error.to_string());
        }
        if self.tool_context.harness.completion() == "unverified"
            && self.last_run_status == RunStatus::Completed
        {
            self.last_run_status = RunStatus::Partial;
        }
        self.last_run_status = if self.tool_context.cancellation.is_cancelled() {
            RunStatus::Cancelled
        } else if result.is_err() && self.last_run_status != RunStatus::BudgetExceeded {
            RunStatus::Failed
        } else {
            self.last_run_status
        };
        if let Err(error) = result {
            self.run_warnings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(error.to_string());
        }
        if !self
            .run_warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
            && self.last_run_status == RunStatus::Completed
        {
            self.last_run_status = RunStatus::Partial;
        }
        let suspended = matches!(
            self.last_run_status,
            RunStatus::Paused | RunStatus::WaitingForInput
        ) && result.is_ok();
        let saved = if suspended {
            self.checkpoint_run()
        } else {
            self.transition(super::continuation::Transition::Finish(
                self.last_run_status,
            ))
        };
        if let Err(error) = saved {
            self.last_run_status = RunStatus::Failed;
            self.run_warnings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("Run checkpoint failed: {error}"));
        }
        let event = if suspended {
            RunEventKind::RunSuspended {
                status: self.last_run_status,
            }
        } else {
            RunEventKind::RunFinished {
                status: self.last_run_status,
            }
        };
        if let Err(error) = self.try_emit_run_event(event) {
            self.last_run_status = RunStatus::Failed;
            self.run_warnings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("Terminal journal write failed: {error}"));
        }
    }

    /// Record an operator-verified resolution. Never called automatically by the model.
    pub fn reconcile_tool_outcome(
        &self,
        run_id: &RunId,
        tool_use_id: &str,
        resolution: &str,
    ) -> crate::error::Result<()> {
        let backend = self
            .history_backend
            .as_ref()
            .ok_or_else(|| crate::error::DysonError::Llm("No execution journal attached".into()))?;
        Self::reconcile_stored_tool_outcome(
            backend.store.clone(),
            &backend.chat_id,
            run_id,
            tool_use_id,
            resolution,
        )?;
        self.tool_context
            .harness
            .clear_pending(&format!("{}:{}", run_id.0, tool_use_id))
    }

    /// Recovery must work before a model or a live Agent has been constructed.
    pub fn reconcile_stored_tool_outcome(
        store: Arc<dyn ChatHistory>,
        chat_id: &str,
        run_id: &RunId,
        tool_use_id: &str,
        resolution: &str,
    ) -> crate::error::Result<()> {
        if resolution.trim().is_empty() {
            return Err(crate::error::DysonError::Llm(
                "A reconciliation requires evidence of the outcome".into(),
            ));
        }
        let events = store.load_run_events(chat_id)?;
        if !super::protocol::unresolved_tool_outcomes(&events)
            .iter()
            .any(|t| &t.run_id == run_id && t.tool_use_id == tool_use_id)
        {
            return Err(crate::error::DysonError::Llm(
                "No matching unresolved invocation".into(),
            ));
        }
        let sequence = events
            .iter()
            .filter(|e| &e.run_id == run_id)
            .map(|e| e.sequence)
            .max()
            .unwrap_or(0)
            + 1;
        store.append_run_event(
            chat_id,
            &RunEvent::new(
                sequence,
                run_id.clone(),
                events
                    .iter()
                    .filter(|e| &e.run_id == run_id)
                    .map(|e| e.turn)
                    .max()
                    .unwrap_or(0),
                RunEventKind::ToolReconciled {
                    tool_use_id: tool_use_id.into(),
                    resolution: resolution.into(),
                },
            ),
        )?;
        let runtime = super::task::TaskRuntime::default();
        runtime.attach(store, chat_id.into());
        runtime.clear_pending(&format!("{}:{}", run_id.0, tool_use_id))?;
        super::task::reconcile_lease(&format!("{}:{}", run_id.0, tool_use_id));
        Ok(())
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
        self.tool_context
            .harness
            .attach(store.clone(), chat_id.clone());
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
