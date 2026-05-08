use crate::controller::Output;
use crate::message::Message;

use super::Agent;

impl Agent {
    pub(super) fn maybe_inject_budget_warning(
        &mut self,
        iteration: usize,
        output: &mut dyn Output,
    ) {
        // Enforce the security-engineer reporting boundary in code so long
        // investigations leave room for a final report before the hard cap.
        const BUDGET_WARN_OFFSET: usize = 20;
        if self.conversation.budget_warning_fired
            || self.max_iterations <= BUDGET_WARN_OFFSET
            || iteration != self.max_iterations - BUDGET_WARN_OFFSET
        {
            return;
        }

        let remaining = self.max_iterations - iteration - 1;
        tracing::info!(iteration, remaining, "injecting budget warning");
        self.conversation.messages.push(Message::user(&format!(
            "[BUDGET WARNING: you have {remaining} iterations left before the \
             agent loop terminates. Stop all further investigation and write \
             the final report now with the findings you already have. Do not \
             start new tool calls that don't directly contribute to the \
             report you are about to emit.]"
        )));
        let _ = output.checkpoint(&crate::tool::CheckpointEvent {
            message: format!(
                "approaching budget — {remaining} iterations left, writing report now"
            ),
            progress: Some(0.9),
        });
        self.conversation.budget_warning_fired = true;
    }
}
