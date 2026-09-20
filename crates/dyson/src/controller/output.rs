//! Rendering contract shared by agent event consumers.
use crate::{
    error::DysonError,
    message::Message,
    tool::{CheckpointEvent, ToolOutput},
};
use std::{future::Future, path::Path, pin::Pin};

// ---------------------------------------------------------------------------
// Output trait
// ---------------------------------------------------------------------------

/// Rendering interface for agent events.
///
/// The agent loop calls these methods as events occur.  Each controller
/// creates an appropriate Output implementation (e.g. writing to stdout,
/// editing chat messages, streaming over HTTP).
///
/// ## Why separate from Controller?
///
/// The agent needs a render target (`&mut dyn Output`) but doesn't know
/// about input sourcing, lifecycle, or access control — those are the
/// controller's job.  Separating Output keeps the agent loop clean.
///
/// ```text
/// Controller (owns lifecycle)
///   │
///   ├── creates Output per session/message
///   │     │
///   │     ▼
///   └── agent.run(input, &mut output)
///         │
///         ├── output.text_delta("Hello")
///         ├── output.tool_use_start(...)
///         ├── output.tool_result(...)
///         └── output.flush()
/// ```
/// Preserve terminal status when an older controller requires a string result.
pub(crate) fn completed_text(outcome: crate::agent::protocol::RunOutcome) -> crate::Result<String> {
    if outcome.status == crate::agent::protocol::RunStatus::Completed {
        Ok(outcome.final_text)
    } else {
        Err(DysonError::Llm(format!(
            "Agent stopped with {:?}: {}",
            outcome.status,
            outcome.warnings.join("; ")
        )))
    }
}

/// Interactive controllers already rendered the question/status. A durable
/// yield is expected there; background completion still requires Completed.
pub(crate) fn interactive_text(
    outcome: crate::agent::protocol::RunOutcome,
) -> crate::Result<String> {
    if matches!(
        outcome.status,
        crate::agent::protocol::RunStatus::Paused
            | crate::agent::protocol::RunStatus::WaitingForInput
    ) {
        Ok(outcome.final_text)
    } else {
        completed_text(outcome)
    }
}

pub trait Output: Send {
    /// A fragment of text from the LLM's response.
    fn text_delta(&mut self, text: &str) -> std::result::Result<(), DysonError>;

    fn human_input_requested(
        &mut self,
        request: &dyson_harness::continuation::HumanInputRequest,
    ) -> std::result::Result<(), DysonError> {
        self.text_delta(&request.question)
    }

    /// A fragment of extended-thinking / chain-of-thought reasoning.
    ///
    /// Default no-op so controllers that don't want to surface reasoning
    /// (terminal, telegram) don't have to opt out.  The HTTP
    /// controller forwards these over SSE so the right-rail can stream
    /// a live reasoning panel.  Not sent to the LLM (the stream_handler
    /// still gathers thinking into `ContentBlock::Thinking` for history
    /// so the model can reference its prior reasoning on the next turn).
    fn thinking_delta(&mut self, text: &str) -> std::result::Result<(), DysonError> {
        let _ = text;
        Ok(())
    }

    /// The LLM is starting a tool call.
    fn tool_use_start(&mut self, id: &str, name: &str) -> std::result::Result<(), DysonError>;

    /// The tool call definition is complete (input JSON fully received).
    fn tool_use_complete(&mut self) -> std::result::Result<(), DysonError>;

    /// A tool has finished executing.
    fn tool_result(&mut self, output: &ToolOutput) -> std::result::Result<(), DysonError>;

    /// Send a file to the user.
    ///
    /// Called by the agent loop when a tool attaches files to its output.
    /// The file is delivered as a side-channel to the user — it does not
    /// appear in the LLM's conversation history.
    ///
    /// Each controller delivers files differently (e.g. printing the path,
    /// sending a document message).
    fn send_file(&mut self, path: &Path) -> std::result::Result<(), DysonError>;

    /// Receive a progress checkpoint event emitted by a tool call.
    ///
    /// Called by the agent loop whenever a tool attaches one or more
    /// `CheckpointEvent`s to its output.  Like `send_file`, this is a
    /// side-channel — the event does not appear in the LLM's conversation
    /// history.
    ///
    /// The default impl drops the event; controllers that need to surface
    /// progress can override it.
    fn checkpoint(&mut self, event: &CheckpointEvent) -> std::result::Result<(), DysonError> {
        let _ = event;
        Ok(())
    }

    /// Receive a rendered artefact emitted by a tool call (e.g. a
    /// security-review report).  Called by the agent loop whenever a
    /// tool attaches one or more `Artefact`s to its output.
    ///
    /// Side-channel: the LLM never sees these.  The HTTP controller
    /// stores the body in-memory and emits an SSE `artefact` event so
    /// the UI renders it in the Artefacts tab.  The default impl drops
    /// the artefact, which is correct for terminal / telegram /
    /// recording / capture controllers.
    fn send_artefact(
        &mut self,
        artefact: &crate::message::Artefact,
    ) -> std::result::Result<(), DysonError> {
        let _ = artefact;
        Ok(())
    }

    /// Give the controller a chance to admit user messages that arrived
    /// while this turn was already running.  The callback is expected to
    /// append and persist each returned message before the controller
    /// acknowledges it as drained.
    fn admit_pending_user_messages<'a>(
        &'a mut self,
        _admit: &'a mut (dyn FnMut(Message) -> std::result::Result<(), DysonError> + Send),
    ) -> Pin<Box<dyn Future<Output = std::result::Result<usize, DysonError>> + Send + 'a>> {
        Box::pin(async { Ok(0) })
    }

    /// A user message was admitted into the running transcript.  HTTP
    /// uses this to reconcile the optimistic queued bubble over SSE;
    /// non-live controllers can ignore it.
    fn user_message(&mut self, _message: &Message) -> std::result::Result<(), DysonError> {
        Ok(())
    }

    /// An error occurred.
    fn error(&mut self, error: &DysonError) -> std::result::Result<(), DysonError>;

    /// Called when the LLM returns a non-retryable error during the agent loop.
    ///
    /// The controller inspects the error and returns a [`LlmRecovery`] action
    /// telling the agent loop how to proceed.  The default implementation
    /// returns [`LlmRecovery::GiveUp`], which propagates the error to the
    /// caller unchanged.
    ///
    /// Controllers may use this hook to send user-facing messages (e.g.
    /// "model doesn't support tools") before returning a recovery action.
    fn on_llm_error(&mut self, error: &DysonError) -> crate::error::LlmRecovery {
        let _ = error;
        crate::error::LlmRecovery::GiveUp
    }

    /// Show or hide a typing indicator.
    ///
    /// Called with `visible = true` just before the LLM call starts (after
    /// the user sends input) and `visible = false` once the first response
    /// token arrives.  Controllers that support a typing indicator should
    /// display/clear it accordingly.
    ///
    /// The default implementation is a no-op.
    fn typing_indicator(&mut self, _visible: bool) -> std::result::Result<(), DysonError> {
        Ok(())
    }

    /// The agent is about to auto-compact its context.  Compaction makes
    /// a full summarisation LLM call that typically adds 5-15 s of
    /// latency the user can otherwise neither see nor predict.  Surface
    /// it so the UI can render a transient notice ("compacting
    /// context…") that the next text fragment naturally clears.
    ///
    /// Default no-op: terminal / telegram / capture controllers don't
    /// need to render compaction state.
    fn compacting_started(
        &mut self,
        _estimated_tokens: usize,
        _threshold: usize,
    ) -> std::result::Result<(), DysonError> {
        Ok(())
    }

    /// Flush any buffered output.
    fn flush(&mut self) -> std::result::Result<(), DysonError>;
}
