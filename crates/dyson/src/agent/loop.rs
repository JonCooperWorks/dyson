use std::path::Path;
use std::sync::Arc;

use crate::controller::Output;
use crate::error::{LlmRecovery, Result};
use crate::message::{Artefact, Message, MessageCostMetadata};
use crate::tool::{CheckpointEvent, ToolOutput};

use super::dream::DreamEvent;
use super::retry::{MAXTOKENS_TOOL_CALL_TRUNCATED, StreamResult};
use super::stream_handler::{self, ToolCall};
use super::{Agent, result_formatter};

struct StreamRetryOutput<'a> {
    inner: &'a mut dyn Output,
    emitted_visible_output: bool,
    // Tracks whether a tool_use START or COMPLETE event has been
    // forwarded.  Once a tool call has been emitted to the parent, we
    // must NOT retry — retrying could double-execute side effects (a
    // bash, a write_file, a send_message).  Plain text deltas, by
    // contrast, are safe to "duplicate" on retry: weaker subagent
    // streams that error after partial text are now retryable, and the
    // duplicate text just reads as the model restating itself.
    emitted_tool_use: bool,
    estimated_output_tokens: usize,
}

impl<'a> StreamRetryOutput<'a> {
    fn new(inner: &'a mut dyn Output) -> Self {
        Self {
            inner,
            emitted_visible_output: false,
            emitted_tool_use: false,
            estimated_output_tokens: 0,
        }
    }

    fn emitted_visible_output(&self) -> bool {
        self.emitted_visible_output
    }

    fn emitted_tool_use(&self) -> bool {
        self.emitted_tool_use
    }
}

impl Output for StreamRetryOutput<'_> {
    fn text_delta(&mut self, text: &str) -> std::result::Result<(), crate::error::DysonError> {
        if !text.is_empty() {
            self.emitted_visible_output = true;
        }
        self.estimated_output_tokens += crate::message::estimate_text_tokens(text);
        self.inner.text_delta(text)
    }

    fn thinking_delta(&mut self, text: &str) -> std::result::Result<(), crate::error::DysonError> {
        self.estimated_output_tokens += crate::message::estimate_text_tokens(text);
        self.inner.thinking_delta(text)
    }

    fn tool_use_start(
        &mut self,
        id: &str,
        name: &str,
    ) -> std::result::Result<(), crate::error::DysonError> {
        self.emitted_visible_output = true;
        self.emitted_tool_use = true;
        self.inner.tool_use_start(id, name)
    }

    fn tool_use_complete(&mut self) -> std::result::Result<(), crate::error::DysonError> {
        self.emitted_visible_output = true;
        self.emitted_tool_use = true;
        self.inner.tool_use_complete()
    }

    fn tool_result(
        &mut self,
        output: &ToolOutput,
    ) -> std::result::Result<(), crate::error::DysonError> {
        self.emitted_visible_output = true;
        self.inner.tool_result(output)
    }

    fn send_file(&mut self, path: &Path) -> std::result::Result<(), crate::error::DysonError> {
        self.emitted_visible_output = true;
        self.inner.send_file(path)
    }

    fn checkpoint(
        &mut self,
        event: &CheckpointEvent,
    ) -> std::result::Result<(), crate::error::DysonError> {
        self.inner.checkpoint(event)
    }

    fn send_artefact(
        &mut self,
        artefact: &Artefact,
    ) -> std::result::Result<(), crate::error::DysonError> {
        self.emitted_visible_output = true;
        self.inner.send_artefact(artefact)
    }

    fn error(
        &mut self,
        error: &crate::error::DysonError,
    ) -> std::result::Result<(), crate::error::DysonError> {
        self.emitted_visible_output = true;
        self.inner.error(error)
    }

    fn on_llm_error(&mut self, error: &crate::error::DysonError) -> LlmRecovery {
        self.inner.on_llm_error(error)
    }

    fn typing_indicator(
        &mut self,
        visible: bool,
    ) -> std::result::Result<(), crate::error::DysonError> {
        self.inner.typing_indicator(visible)
    }

    fn flush(&mut self) -> std::result::Result<(), crate::error::DysonError> {
        self.inner.flush()
    }
}

struct IterationResponse {
    tool_mode: crate::llm::ToolMode,
    input_tokens: Option<usize>,
    assistant_msg: Message,
    tool_calls: Vec<ToolCall>,
    output_tokens: usize,
    stop_reason: crate::llm::stream::StopReason,
    cost_metadata: Option<MessageCostMetadata>,
}

enum IterationFlow {
    Ready(Box<IterationResponse>),
    RetryOuter,
    Cancelled,
}

struct StreamCompletion {
    assistant_msg: Message,
    tool_calls: Vec<ToolCall>,
    output_tokens: usize,
    stop_reason: crate::llm::stream::StopReason,
}

enum StreamAttempt {
    Complete(Box<StreamCompletion>),
    Retry,
    Cancelled,
}

#[derive(Default)]
struct TurnProgress {
    final_text: String,
    hit_max_iterations: bool,
    any_text_streamed: bool,
    last_streamed_text: String,
    continuation_prefix: String,
}

enum LoopControl {
    Continue,
    Break,
}

impl Agent {
    async fn start_stream_attempt(
        &mut self,
        iteration: usize,
        attempt: usize,
        skill_fragments: &str,
        recovered_this_turn: &mut bool,
        output: &mut dyn Output,
    ) -> Result<Option<crate::llm::StreamResponse>> {
        if !self.conversation.token_budget.has_budget() {
            self.last_run_status = super::protocol::RunStatus::BudgetExceeded;
            return Err(crate::error::DysonError::Llm(
                "token budget exhausted".into(),
            ));
        }
        self.emit_run_event(super::protocol::RunEventKind::LlmAttemptStarted {
            iteration,
            attempt,
        });
        match self
            .stream_with_retry(skill_fragments, recovered_this_turn, output)
            .await
        {
            StreamResult::Response(response) => Ok(Some(response)),
            StreamResult::Recovered(error) => {
                self.conversation.token_budget.llm_calls += 1;
                self.emit_failed_attempt(iteration, &error, false);
                Ok(None)
            }
            StreamResult::Error(error) => {
                self.conversation.token_budget.llm_calls += 1;
                self.emit_failed_attempt(iteration, &error, false);
                Err(error)
            }
        }
    }

    fn emit_failed_attempt(
        &self,
        iteration: usize,
        error: &crate::error::DysonError,
        after_tool_use: bool,
    ) {
        self.emit_run_event(super::protocol::RunEventKind::LlmAttemptFailed {
            iteration,
            error_kind: llm_error_kind(error).to_string(),
            retryable: crate::llm::is_retryable(error),
            after_tool_use,
        });
    }

    async fn wait_for_retry(&self, delay_ms: u64) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => false,
            _ = self.tool_context.cancellation.cancelled() => {
                tracing::info!("retry backoff interrupted — agent cancelled");
                true
            }
        }
    }

    async fn process_stream_attempt(
        &mut self,
        response: crate::llm::StreamResponse,
        iteration: usize,
        stream_error_attempts: usize,
        output: &mut dyn Output,
    ) -> Result<StreamAttempt> {
        let observed_input = response.input_tokens;
        let (stream_result, emitted_visible_output, emitted_tool_use, estimated_output_tokens) = {
            let mut retry_output = StreamRetryOutput::new(output);
            let stream_result =
                stream_handler::process_stream(response.stream, &mut retry_output).await;
            (
                stream_result,
                retry_output.emitted_visible_output(),
                retry_output.emitted_tool_use(),
                retry_output.estimated_output_tokens,
            )
        };
        if let Some(reservation) = self.pending_task_budget.take()
            && let Ok((_, _, tokens, _)) = &stream_result
        {
            reservation.settle(observed_input, *tokens);
        }
        self.tool_context.harness.checkpoint()?;
        let transport_retryable_mid_stream =
            matches!(&stream_result, Err(crate::error::DysonError::Http(_)));
        match stream_result {
            Ok((assistant_msg, tool_calls, output_tokens, stop_reason)) => {
                Ok(StreamAttempt::Complete(Box::new(StreamCompletion {
                    assistant_msg,
                    tool_calls,
                    output_tokens,
                    stop_reason,
                })))
            }
            Err(error)
                if crate::llm::is_retryable(&error)
                    && stream_error_attempts < self.max_retries
                    && !emitted_tool_use
                    && (!emitted_visible_output || transport_retryable_mid_stream) =>
            {
                let _ = self
                    .conversation
                    .token_budget
                    .record(estimated_output_tokens);
                self.emit_failed_attempt(iteration, &error, emitted_tool_use);
                let delay_ms = compute_backoff_ms(stream_error_attempts);
                tracing::warn!(
                    attempt = stream_error_attempts + 1,
                    max = self.max_retries,
                    delay_ms,
                    error = %error,
                    mid_stream = emitted_visible_output,
                    "LLM stream failed — retrying"
                );
                if self.wait_for_retry(delay_ms).await {
                    Ok(StreamAttempt::Cancelled)
                } else {
                    Ok(StreamAttempt::Retry)
                }
            }
            Err(error) => {
                let _ = self
                    .conversation
                    .token_budget
                    .record(estimated_output_tokens);
                self.emit_failed_attempt(iteration, &error, emitted_tool_use);
                Err(error)
            }
        }
    }

    async fn stream_iteration(
        &mut self,
        iteration: usize,
        skill_fragments: &str,
        recovered_this_turn: &mut bool,
        output: &mut dyn Output,
    ) -> Result<IterationFlow> {
        let mut empty_attempts = 0;
        let mut stream_error_attempts = 0;
        loop {
            let Some(response) = self
                .start_stream_attempt(
                    iteration,
                    empty_attempts + stream_error_attempts,
                    skill_fragments,
                    recovered_this_turn,
                    output,
                )
                .await?
            else {
                return Ok(IterationFlow::RetryOuter);
            };
            let tool_mode = response.tool_mode;
            let input_tokens = response.input_tokens;
            if let Some(tokens) = input_tokens {
                self.conversation.token_budget.record_input(tokens);
            }
            let audit_id = response.swarm_llm_audit_id;
            let provider = response.provider.clone();
            let model = response.model.clone();
            tracing::info!(tool_mode = ?tool_mode, input_tokens = ?input_tokens, "streaming response");

            let attempt = self
                .process_stream_attempt(response, iteration, stream_error_attempts, output)
                .await?;
            let StreamAttempt::Complete(completion) = attempt else {
                match attempt {
                    StreamAttempt::Retry => {
                        stream_error_attempts += 1;
                        continue;
                    }
                    StreamAttempt::Cancelled => return Ok(IterationFlow::Cancelled),
                    StreamAttempt::Complete(_) => unreachable!(),
                }
            };
            let StreamCompletion {
                assistant_msg,
                tool_calls,
                output_tokens,
                stop_reason,
            } = *completion;
            self.emit_run_event(super::protocol::RunEventKind::LlmAttemptCompleted {
                iteration,
                output_tokens,
                tool_calls: tool_calls.len(),
            });

            let _ = self.conversation.token_budget.record(output_tokens);
            let empty = assistant_msg.last_text().is_none()
                && tool_calls.is_empty()
                && tool_mode != crate::llm::ToolMode::Observe;
            if empty
                && empty_attempts < self.max_retries
                && self.conversation.token_budget.has_budget()
            {
                let delay_ms = compute_backoff_ms(empty_attempts);
                tracing::warn!(
                    attempt = empty_attempts + 1,
                    max = self.max_retries,
                    delay_ms,
                    "LLM returned no text and no tool calls — retrying"
                );
                if self.wait_for_retry(delay_ms).await {
                    return Ok(IterationFlow::Cancelled);
                }
                empty_attempts += 1;
                continue;
            }

            let cost_metadata = audit_id.map(|swarm_llm_audit_id| MessageCostMetadata {
                swarm_llm_audit_id: Some(swarm_llm_audit_id),
                display_cost_usd: None,
                cost_source: None,
                cost_finalized_at: None,
                provider,
                model,
                input_tokens: input_tokens.and_then(|value| i64::try_from(value).ok()),
                output_tokens: i64::try_from(output_tokens).ok(),
                key_source: None,
            });
            return Ok(IterationFlow::Ready(Box::new(IterationResponse {
                tool_mode,
                input_tokens,
                assistant_msg,
                tool_calls,
                output_tokens,
                stop_reason,
                cost_metadata,
            })));
        }
    }

    fn finish_text_response(
        &mut self,
        assistant_msg: Message,
        progress: &mut TurnProgress,
        output: &mut dyn Output,
    ) -> Result<()> {
        progress.final_text = if let Some(text) = assistant_msg.last_text() {
            text.to_string()
        } else if progress.any_text_streamed {
            tracing::warn!("LLM returned no text on final iteration — reusing last streamed text");
            progress.last_streamed_text.clone()
        } else {
            tracing::warn!("LLM returned no text and no tool calls — sending fallback");
            let fallback = "I wasn't able to generate a response. Please try again.";
            output.text_delta(fallback)?;
            fallback.to_string()
        };
        if !progress.continuation_prefix.is_empty() {
            progress.final_text =
                format!("{}{}", progress.continuation_prefix, progress.final_text);
        }
        self.last_run_status = if assistant_msg.last_text().is_some() {
            super::protocol::RunStatus::Completed
        } else {
            super::protocol::RunStatus::Partial
        };
        self.conversation.messages.push(assistant_msg);
        output.flush()
    }

    async fn handle_iteration_response(
        &mut self,
        response: IterationResponse,
        iteration: usize,
        progress: &mut TurnProgress,
        output: &mut dyn Output,
    ) -> Result<LoopControl> {
        let IterationResponse {
            tool_mode,
            input_tokens,
            mut assistant_msg,
            tool_calls,
            output_tokens,
            stop_reason,
            cost_metadata,
        } = response;
        if let Some(cost_metadata) = cost_metadata {
            assistant_msg.cost = Some(finalize_cost_metadata(cost_metadata).await);
        }
        let _ = (input_tokens, output_tokens);
        if self
            .conversation
            .token_budget
            .max_output_tokens
            .is_some_and(|max| self.conversation.token_budget.output_tokens_used > max)
        {
            let error = crate::error::DysonError::Llm("token budget exceeded".into());
            self.conversation.messages.push(assistant_msg);
            self.last_run_status = super::protocol::RunStatus::BudgetExceeded;
            tracing::warn!(
                used = self.conversation.token_budget.output_tokens_used,
                "token budget exceeded — stopping agent loop"
            );
            output.error(&error)?;
            return Ok(LoopControl::Break);
        }
        if let Some(text) = assistant_msg.last_text() {
            progress.any_text_streamed = true;
            progress.last_streamed_text = text.to_string();
        }
        self.log_response(&assistant_msg, &tool_calls);

        if stop_reason == crate::llm::stream::StopReason::MaxTokens
            && tool_calls.is_empty()
            && tool_mode != crate::llm::ToolMode::Observe
        {
            tracing::warn!("response truncated by max_tokens — injecting continuation prompt");
            if let Some(text) = assistant_msg.last_text() {
                progress.continuation_prefix.push_str(text);
            }
            self.conversation.messages.push(assistant_msg);
            self.conversation.messages.push(Message::user(
                "[Your previous response was cut off because it exceeded the \
                 output token limit. Please continue exactly where you left off.]",
            ));
            return Ok(LoopControl::Continue);
        }

        let truncated_tool_call = stop_reason == crate::llm::stream::StopReason::MaxTokens
            && tool_mode != crate::llm::ToolMode::Observe
            && tool_calls
                .iter()
                .any(|call| call.input.get("_parse_error").is_some());
        if truncated_tool_call {
            let names: Vec<&str> = tool_calls
                .iter()
                .filter_map(|call| {
                    call.input
                        .get("_parse_error")
                        .is_some()
                        .then_some(call.name.as_str())
                })
                .collect();
            tracing::warn!(
                tools = ?names,
                "tool call JSON truncated by max_tokens — redirecting LLM to split work"
            );
            self.conversation.messages.push(assistant_msg);
            self.conversation
                .messages
                .push(Message::user(MAXTOKENS_TOOL_CALL_TRUNCATED));
            return Ok(LoopControl::Continue);
        }

        if tool_calls.is_empty() || tool_mode == crate::llm::ToolMode::Observe {
            self.finish_text_response(assistant_msg, progress, output)?;
            return Ok(LoopControl::Break);
        }
        self.conversation.messages.push(assistant_msg);
        self.execute_tool_calls(&tool_calls, output).await?;
        self.admit_pending_user_messages(output).await?;
        self.maybe_inject_budget_warning(iteration, output);
        if iteration == self.max_iterations - 1 {
            tracing::warn!(
                max = self.max_iterations,
                "agent hit maximum iterations — requesting summary"
            );
            progress.hit_max_iterations = true;
        }
        Ok(LoopControl::Continue)
    }

    /// Inner agent loop shared by [`run()`], [`run_with_blocks()`], and
    /// [`run_with_attachments()`].
    ///
    /// Assumes the caller has already pushed the user message to
    /// `self.conversation.messages`.
    pub(super) async fn run_inner(&mut self, output: &mut dyn Output) -> Result<String> {
        let deadline = self
            .tool_context
            .harness
            .budget
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remaining_time();
        let deadline = match deadline {
            Ok(d) => d,
            Err(e) => {
                self.last_run_status = super::protocol::RunStatus::BudgetExceeded;
                return Err(e);
            }
        };
        let runtime = self.tool_context.harness.clone();
        let result = tokio::time::timeout(
            deadline,
            super::task::budget::ACTIVE.scope(runtime, self.run_inner_impl(output)),
        )
        .await;
        match result {
            Ok(value) => value,
            Err(_) => {
                self.pending_task_budget.take();
                self.last_run_status = super::protocol::RunStatus::BudgetExceeded;
                Err(crate::error::DysonError::Llm(
                    "task elapsed-time budget exhausted".into(),
                ))
            }
        }
    }

    async fn run_inner_impl(&mut self, output: &mut dyn Output) -> Result<String> {
        self.conversation.turn_count += 1;
        self.conversation.budget_warning_fired = false;

        let mut progress = TurnProgress::default();

        let skill_fragments = format!(
            "{}\nTask checkpoint: {}",
            self.collect_skill_context().await,
            self.tool_context.harness.resume_summary()
        );

        let turn_system_prompt: Arc<str> = if skill_fragments.is_empty() {
            Arc::clone(&self.system_prompt)
        } else {
            let mut prompt =
                String::with_capacity(self.system_prompt.len() + skill_fragments.len());
            prompt.push_str(&self.system_prompt);
            prompt.push_str(&skill_fragments);
            Arc::from(prompt)
        };

        let mut recovered_this_turn = false;

        let mut progress_nudged = false;
        'iter: for iteration in 0..self.max_iterations {
            if self
                .tool_context
                .harness
                .budget
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .exhausted()
            {
                self.last_run_status = super::protocol::RunStatus::BudgetExceeded;
                break;
            }
            let stagnant = self.tool_context.harness.stagnant_calls();
            if stagnant >= 16 {
                self.last_run_status = super::protocol::RunStatus::Partial;
                progress.final_text = "Task paused: repeated attempts have produced no new successful evidence. Review the durable task checkpoint and blockers before continuing.".into();
                output.text_delta(&progress.final_text)?;
                break;
            }
            if stagnant >= 8 && !progress_nudged {
                self.conversation.messages.push(Message::user("TASK STALLED: eight attempts produced no new successful evidence, even across changed commands. Re-read task_control status, change the hypothesis or strategy, and verify a concrete criterion. Further unproductive work will pause this run."));
                progress_nudged = true;
            }
            if !self.conversation.token_budget.has_budget() {
                self.last_run_status = super::protocol::RunStatus::BudgetExceeded;
                break;
            }
            let budget = &self.conversation.token_budget;
            if budget.summary_reserve() > 0 && budget.remaining() <= budget.summary_reserve() {
                self.last_run_status = super::protocol::RunStatus::BudgetExceeded;
                progress.final_text = self
                    .summarize_on_max_iterations(&skill_fragments, output)
                    .await?;
                break;
            }
            // Check for cooperative cancellation (used by /stop).
            if self.tool_context.cancellation.is_cancelled() {
                tracing::info!("agent cancelled — breaking loop");
                self.last_run_status = super::protocol::RunStatus::Cancelled;
                break;
            }

            self.auto_compact_if_needed(&turn_system_prompt, output)
                .await;
            self.log_iteration(iteration);

            output.typing_indicator(true)?;

            let response = match self
                .stream_iteration(
                    iteration,
                    &skill_fragments,
                    &mut recovered_this_turn,
                    output,
                )
                .await?
            {
                IterationFlow::Ready(response) => response,
                IterationFlow::RetryOuter => {
                    if iteration + 1 == self.max_iterations {
                        progress.hit_max_iterations = true;
                    }
                    continue 'iter;
                }
                IterationFlow::Cancelled => break 'iter,
            };
            if matches!(
                self.handle_iteration_response(*response, iteration, &mut progress, output)
                    .await?,
                LoopControl::Break
            ) {
                break;
            }
            if iteration + 1 == self.max_iterations {
                progress.hit_max_iterations = true;
            }
        }

        if self.max_iterations == 0 {
            progress.hit_max_iterations = true;
        }
        if progress.hit_max_iterations {
            self.last_run_status = super::protocol::RunStatus::IterationLimit;
            progress.final_text = self
                .summarize_on_max_iterations(&skill_fragments, output)
                .await?;
            if !progress.continuation_prefix.is_empty() {
                progress.final_text =
                    format!("{}{}", progress.continuation_prefix, progress.final_text);
            }
        }

        output.flush()?;
        // A TurnComplete snapshot must include the assistant's final response
        // and every committed tool result.  Firing this at turn start taught
        // memory dreams from incomplete conversations.
        self.fire_dreams(DreamEvent::TurnComplete {
            turn_count: self.conversation.turn_count,
        });
        Ok(progress.final_text)
    }

    async fn admit_pending_user_messages(&mut self, output: &mut dyn Output) -> Result<()> {
        let mut admitted = Vec::new();
        let count = {
            let mut admit =
                |message: Message| -> std::result::Result<(), crate::error::DysonError> {
                    self.conversation.messages.push(message.clone());
                    self.persist();
                    admitted.push(message);
                    Ok(())
                };
            output.admit_pending_user_messages(&mut admit).await?
        };
        if count == 0 {
            return Ok(());
        }
        for message in admitted {
            output.user_message(&message)?;
        }
        Ok(())
    }

    /// Collect ephemeral per-turn context from all skills.
    async fn collect_skill_context(&self) -> String {
        let mut fragments = String::new();
        for skill in &self.skills {
            match skill.before_turn().await {
                Ok(Some(fragment)) => {
                    fragments.push_str("\n\n");
                    fragments.push_str(&fragment);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        skill = skill.name(),
                        error = %e,
                        "skill before_turn failed — continuing without its context"
                    );
                }
            }
        }
        if let Some(prompt) = self.advisor_prompt {
            fragments.push_str(prompt);
        }
        fragments
    }

    /// Auto-compact if estimated context tokens exceed the threshold.
    async fn auto_compact_if_needed(&mut self, turn_system_prompt: &str, output: &mut dyn Output) {
        if self.conversation.messages.len() <= self.compaction_config.protect_head {
            return;
        }
        let threshold = self.compaction_config.threshold();
        let estimated_tokens = self.estimate_context_tokens(turn_system_prompt);
        if estimated_tokens > threshold {
            tracing::info!(
                estimated_tokens,
                threshold,
                messages = self.conversation.messages.len(),
                "estimated context tokens exceed compaction threshold — compacting"
            );
            // Surface to the controller so the UI can show a transient
            // "compacting context…" notice instead of a silent stall
            // while the summarisation LLM call runs.
            if let Err(e) = output.compacting_started(estimated_tokens, threshold) {
                tracing::warn!(error = %e, "failed to emit compacting_started event");
            }
            if let Err(e) = self.compact(output).await {
                tracing::warn!(
                    error = %e,
                    "auto-compaction failed — continuing with full history"
                );
            }
        }
    }

    /// Log the start of an LLM iteration.
    fn log_iteration(&self, iteration: usize) {
        tracing::info!(
            iteration,
            model = self.config.model,
            messages = self.conversation.messages.len(),
            tools_enabled = !self.tool_registry.disabled,
            tool_count = self.tool_registry.definitions.len(),
            "starting LLM call"
        );

        if tracing::enabled!(tracing::Level::DEBUG) {
            for (i, msg) in self.conversation.messages.iter().enumerate() {
                let role = match msg.role {
                    crate::message::Role::User => "user",
                    crate::message::Role::Assistant => "assistant",
                };
                let block_summary: Vec<String> = msg
                    .content
                    .iter()
                    .map(|b| match b {
                        crate::message::ContentBlock::Text { text } => {
                            format!("text({})", text.len())
                        }
                        crate::message::ContentBlock::ToolUse { name, .. } => {
                            format!("tool_use({name})")
                        }
                        crate::message::ContentBlock::ToolResult {
                            tool_use_id,
                            is_error,
                            ..
                        } => {
                            format!("tool_result({tool_use_id}, error={is_error})")
                        }
                        crate::message::ContentBlock::Image { .. } => "image".to_string(),
                        crate::message::ContentBlock::Document { .. } => "document".to_string(),
                        crate::message::ContentBlock::Thinking { .. } => "thinking".to_string(),
                        crate::message::ContentBlock::Artefact { kind, .. } => {
                            format!("artefact({kind:?})")
                        }
                    })
                    .collect();
                tracing::debug!(
                    msg_index = i,
                    role,
                    blocks = ?block_summary,
                    "message in context"
                );
            }
        }
    }

    /// Stream an LLM response, invoking controller recovery on failure.
    ///
    /// Transient failures (429, overloaded, transport errors) are retried
    /// *inside* the `LlmClient` by `RetryingLlmClient` with exponential
    /// backoff — by the time an error reaches this function, the client
    /// has already exhausted its retries.  All that's left for this layer
    /// is to ask the controller whether a non-retryable error (e.g. "model
    /// doesn't support tools") should trigger a `RetryWithoutTools` or
    /// `RetryWithoutImages` recovery.
    async fn stream_with_retry(
        &mut self,
        skill_fragments: &str,
        recovered_this_turn: &mut bool,
        output: &mut dyn Output,
    ) -> StreamResult {
        let tools_for_llm = self.tool_registry.definitions_for_llm();

        // The agent's own sliding-window limiter is not a provider error:
        // we know locally when the window frees, so wait it out (bounded)
        // instead of hard-failing the whole turn.  Provider-side 429s are
        // handled separately by RetryingLlmClient.
        let client = {
            let mut waited = std::time::Duration::ZERO;
            loop {
                match self.client.access() {
                    Ok(c) => break c,
                    Err(crate::error::DysonError::RateLimit { limit, window_secs }) => {
                        // The oldest event ages out of the sliding window
                        // after at most `window_secs`, so a bound of one
                        // full window (plus slack, capped for sanity)
                        // guarantees a slot frees — unless another caller
                        // keeps stealing it, at which point we give up.
                        let max_wait = std::time::Duration::from_secs(window_secs.max(1))
                            .saturating_add(std::time::Duration::from_secs(1))
                            .min(std::time::Duration::from_secs(300));
                        if waited >= max_wait {
                            return StreamResult::Error(crate::error::DysonError::RateLimit {
                                limit,
                                window_secs,
                            });
                        }
                        let poll = std::time::Duration::from_millis(
                            (window_secs.saturating_mul(1000) / 10).clamp(50, 1000),
                        );
                        tracing::info!(
                            window_secs,
                            waited_ms = waited.as_millis() as u64,
                            "self-imposed rate limit hit — waiting for the window to free"
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(poll) => {}
                            _ = self.tool_context.cancellation.cancelled() => {
                                tracing::info!("rate-limit wait interrupted — agent cancelled");
                                return StreamResult::Error(crate::error::DysonError::RateLimit {
                                    limit,
                                    window_secs,
                                });
                            }
                        }
                        waited += poll;
                    }
                    Err(e) => return StreamResult::Error(e),
                }
            }
        };

        let mut config = self.budgeted_config();
        let budget = &self.conversation.token_budget;
        config.max_tokens = config.max_tokens.min(
            budget
                .remaining()
                .saturating_sub(budget.summary_reserve())
                .max(1)
                .min(u32::MAX as usize) as u32,
        );
        let estimated_input = self
            .conversation
            .messages
            .iter()
            .map(Message::estimate_tokens)
            .sum::<usize>()
            + crate::message::estimate_text_tokens(&self.system_prompt)
            + crate::message::estimate_text_tokens(skill_fragments)
            + self.tool_registry.cached_tokens;
        let (reservation, max_output) = match super::task::budget::Reservation::reserve(
            self.tool_context.harness.budget.clone(),
            &config.model,
            estimated_input as u64,
            config.max_tokens,
        ) {
            Ok(value) => value,
            Err(error) => {
                self.last_run_status = super::protocol::RunStatus::BudgetExceeded;
                return StreamResult::Error(error);
            }
        };
        config.max_tokens = max_output;
        self.pending_task_budget = Some(reservation);
        if let Err(error) = self.tool_context.harness.checkpoint() {
            self.pending_task_budget.take();
            return StreamResult::Error(error);
        }
        let err = match client
            .stream(
                &self.conversation.messages,
                &self.system_prompt,
                skill_fragments,
                tools_for_llm,
                &self.tool_registry.tools,
                &config,
            )
            .await
        {
            Ok(s) => return StreamResult::Response(s),
            Err(e) => e,
        };
        self.pending_task_budget.take();
        let _ = self.tool_context.harness.checkpoint();

        if *recovered_this_turn {
            return StreamResult::Error(err);
        }
        let action = output.on_llm_error(&err);
        if action == LlmRecovery::GiveUp {
            return StreamResult::Error(err);
        }

        let user_msg = self.pop_last_message();
        match action {
            LlmRecovery::RetryWithoutTools => {
                tracing::warn!("controller requested retry without tools");
                self.disable_tools();
                self.strip_tool_history();
            }
            LlmRecovery::RetryWithoutImages => {
                tracing::warn!("controller requested retry without images");
                self.strip_images();
            }
            LlmRecovery::GiveUp => unreachable!(),
        }
        if let Some(msg) = user_msg {
            self.conversation.messages.push(msg);
        }
        *recovered_this_turn = true;
        StreamResult::Recovered(err)
    }

    /// Log a summary of the assistant response.
    fn log_response(&self, assistant_msg: &Message, tool_calls: &[ToolCall]) {
        if let Some(text) = assistant_msg.last_text() {
            let preview = result_formatter::preview(text, 500);
            tracing::info!(
                response_len = text.len(),
                response_preview = preview,
                tool_calls = tool_calls.len(),
                "assistant response"
            );
        } else {
            tracing::info!(
                tool_calls = tool_calls.len(),
                "assistant response (no text)"
            );
        }
    }
}

fn llm_error_kind(error: &crate::error::DysonError) -> &'static str {
    match error {
        crate::error::DysonError::Llm(_) => "llm",
        crate::error::DysonError::LlmRateLimit { .. } => "provider_rate_limit",
        crate::error::DysonError::LlmOverloaded { .. } => "provider_overloaded",
        crate::error::DysonError::RateLimit { .. } => "local_rate_limit",
        crate::error::DysonError::Http(_) => "transport",
        crate::error::DysonError::Cancelled => "cancelled",
        _ => "internal",
    }
}

async fn finalize_cost_metadata(mut metadata: MessageCostMetadata) -> MessageCostMetadata {
    let Some(audit_id) = metadata.swarm_llm_audit_id else {
        return metadata;
    };
    match crate::swarm_cost::lookup_runtime_display_metadata(audit_id).await {
        Ok(Some(finalized)) => {
            crate::message_cost_backfill::merge_cost(&mut metadata, finalized);
        }
        Ok(None) => {}
        Err(err) => {
            tracing::debug!(audit_id, error = %err, "Swarm cost lookup failed");
        }
    }
    metadata
}

/// Exponential backoff with up-to-half jitter, starting at 1s.  Shared between
/// the stream-error and empty-response retry paths via [`backoff_with_jitter`].
fn compute_backoff_ms(attempt: usize) -> u64 {
    crate::util::backoff_with_jitter(1000, attempt)
}

#[cfg(test)]
mod backoff_tests {
    use super::compute_backoff_ms;

    #[test]
    fn backoff_first_attempt_is_at_least_one_second() {
        let v = compute_backoff_ms(0);
        assert!(v >= 1000, "first attempt must be ≥1s, got {v}");
        assert!(v <= 1500, "first attempt jitter capped at +50%, got {v}");
    }

    #[test]
    fn backoff_grows_exponentially() {
        // Floor of the band at attempt n is 1000 * 2^n; ceiling is +50%.
        for n in 0..5 {
            let lo = 1000u64 * 2u64.pow(n);
            let hi = lo + lo / 2;
            let v = compute_backoff_ms(n as usize);
            assert!(
                (lo..=hi).contains(&v),
                "attempt {n}: {v} outside [{lo},{hi}]"
            );
        }
    }
}
