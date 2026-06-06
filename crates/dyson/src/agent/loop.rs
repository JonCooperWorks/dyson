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
}

impl<'a> StreamRetryOutput<'a> {
    fn new(inner: &'a mut dyn Output) -> Self {
        Self {
            inner,
            emitted_visible_output: false,
        }
    }

    fn emitted_visible_output(&self) -> bool {
        self.emitted_visible_output
    }
}

impl Output for StreamRetryOutput<'_> {
    fn text_delta(&mut self, text: &str) -> std::result::Result<(), crate::error::DysonError> {
        if !text.is_empty() {
            self.emitted_visible_output = true;
        }
        self.inner.text_delta(text)
    }

    fn thinking_delta(&mut self, text: &str) -> std::result::Result<(), crate::error::DysonError> {
        self.inner.thinking_delta(text)
    }

    fn tool_use_start(
        &mut self,
        id: &str,
        name: &str,
    ) -> std::result::Result<(), crate::error::DysonError> {
        self.emitted_visible_output = true;
        self.inner.tool_use_start(id, name)
    }

    fn tool_use_complete(&mut self) -> std::result::Result<(), crate::error::DysonError> {
        self.emitted_visible_output = true;
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

impl Agent {
    /// Inner agent loop shared by [`run()`], [`run_with_blocks()`], and
    /// [`run_with_attachments()`].
    ///
    /// Assumes the caller has already pushed the user message to
    /// `self.conversation.messages`.
    pub(super) async fn run_inner(&mut self, output: &mut dyn Output) -> Result<String> {
        self.conversation.turn_count += 1;
        self.conversation.budget_warning_fired = false;

        self.fire_dreams(DreamEvent::TurnComplete {
            turn_count: self.conversation.turn_count,
        });

        let mut final_text = String::new();
        let mut hit_max_iterations = false;
        let mut any_text_streamed = false;
        // Remember the most recent text the LLM streamed this turn so we can
        // surface it if the final iteration comes back empty after retries.
        let mut last_streamed_text = String::new();
        // Accumulate partial assistant text across MaxTokens-forced
        // continuations.  `final_text` only holds the *last* turn's text,
        // so without this buffer every chunk before the final one is lost
        // from the return value (the conversation history still has them).
        let mut continuation_prefix = String::new();

        let skill_fragments = self.collect_skill_context().await;

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

        'iter: for iteration in 0..self.max_iterations {
            // Check for cooperative cancellation (used by /stop).
            if self.tool_context.cancellation.is_cancelled() {
                tracing::info!("agent cancelled — breaking loop");
                break;
            }

            self.auto_compact_if_needed(&turn_system_prompt, output)
                .await;
            self.log_iteration(iteration);

            output.typing_indicator(true)?;

            // Stream LLM response with retry/backoff.  If the LLM returns no
            // text and no tool calls, retry the request per our retry policy
            // without advancing the iteration counter.
            let mut empty_attempts: usize = 0;
            let mut stream_error_attempts: usize = 0;
            let (
                tool_mode,
                input_tokens,
                mut assistant_msg,
                tool_calls,
                output_tokens,
                stop_reason,
                cost_metadata,
            ) = loop {
                let response = match self
                    .stream_with_retry(&skill_fragments, &mut recovered_this_turn, output)
                    .await
                {
                    StreamResult::Response(r) => r,
                    StreamResult::Recovered => continue 'iter,
                    StreamResult::Error(e) => return Err(e),
                };

                let tool_mode = response.tool_mode;
                let input_tokens = response.input_tokens;
                let audit_id = response.swarm_llm_audit_id;
                let provider = response.provider.clone();
                let model = response.model.clone();

                tracing::info!(
                    tool_mode = ?tool_mode,
                    input_tokens = ?input_tokens,
                    "streaming response"
                );

                let (stream_result, emitted_visible_output) = {
                    let mut retry_output = StreamRetryOutput::new(output);
                    let stream_result =
                        stream_handler::process_stream(response.stream, &mut retry_output).await;
                    (stream_result, retry_output.emitted_visible_output())
                };

                let (assistant_msg, tool_calls, output_tokens, stop_reason) = match stream_result {
                    Ok(result) => result,
                    Err(e)
                        if crate::llm::is_retryable(&e)
                            && stream_error_attempts < self.max_retries
                            && !emitted_visible_output =>
                    {
                        let delay_ms = compute_backoff_ms(stream_error_attempts);
                        tracing::warn!(
                            attempt = stream_error_attempts + 1,
                            max = self.max_retries,
                            delay_ms,
                            error = %e,
                            "LLM stream failed before visible output — retrying"
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                            _ = self.tool_context.cancellation.cancelled() => {
                                tracing::info!("retry backoff interrupted — agent cancelled");
                                break 'iter;
                            }
                        }
                        stream_error_attempts += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                };

                // Empty responses (no text, no tool calls) can happen
                // transiently — retry per the same policy we use for network
                // failures.  Skip for Observe mode, where tool calls in the
                // stream are informational and absence doesn't indicate an
                // empty reply.
                let is_empty = assistant_msg.last_text().is_none()
                    && tool_calls.is_empty()
                    && tool_mode != crate::llm::ToolMode::Observe;
                if is_empty && empty_attempts < self.max_retries {
                    let delay_ms = compute_backoff_ms(empty_attempts);
                    tracing::warn!(
                        attempt = empty_attempts + 1,
                        max = self.max_retries,
                        delay_ms,
                        "LLM returned no text and no tool calls — retrying"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                        _ = self.tool_context.cancellation.cancelled() => {
                            tracing::info!("retry backoff interrupted — agent cancelled");
                            break 'iter;
                        }
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
                    input_tokens: input_tokens.and_then(|n| i64::try_from(n).ok()),
                    output_tokens: i64::try_from(output_tokens).ok(),
                    key_source: None,
                });

                break (
                    tool_mode,
                    input_tokens,
                    assistant_msg,
                    tool_calls,
                    output_tokens,
                    stop_reason,
                    cost_metadata,
                );
            };

            if let Some(cost_metadata) = cost_metadata {
                assistant_msg.cost = Some(finalize_cost_metadata(cost_metadata).await);
            }

            if let Some(input_tokens) = input_tokens {
                self.conversation.token_budget.record_input(input_tokens);
            }

            if let Err(e) = self.conversation.token_budget.record(output_tokens) {
                self.conversation.messages.push(assistant_msg);
                tracing::warn!(
                    used = self.conversation.token_budget.output_tokens_used,
                    "token budget exceeded — stopping agent loop"
                );
                output.error(&e)?;
                break;
            }

            if let Some(text) = assistant_msg.last_text() {
                any_text_streamed = true;
                last_streamed_text = text.to_string();
            }

            self.log_response(&assistant_msg, &tool_calls);

            // MaxTokens with no tool calls means the response was truncated
            // mid-generation.  Push the partial message into history and
            // inject a continuation prompt so the LLM picks up where it
            // left off.  Skip for Observe mode (provider manages its own
            // loop) and when tool calls are present (they'll execute
            // normally and the next iteration continues naturally).
            if stop_reason == crate::llm::stream::StopReason::MaxTokens
                && tool_calls.is_empty()
                && tool_mode != crate::llm::ToolMode::Observe
            {
                tracing::warn!("response truncated by max_tokens — injecting continuation prompt");
                if let Some(text) = assistant_msg.last_text() {
                    continuation_prefix.push_str(text);
                }
                self.conversation.messages.push(assistant_msg);
                self.conversation.messages.push(Message::user(
                    "[Your previous response was cut off because it exceeded the \
                     output token limit. Please continue exactly where you left off.]",
                ));
                continue;
            }

            // MaxTokens WITH tool calls: if `finalize_tool_call` marked any
            // call with `_parse_error`, the JSON was cut off mid-argument.
            // Dispatching it would waste a round-trip and the model would
            // re-emit the same oversized payload.  Redirect it to a smaller
            // strategy instead.
            if stop_reason == crate::llm::stream::StopReason::MaxTokens
                && tool_mode != crate::llm::ToolMode::Observe
                && tool_calls
                    .iter()
                    .any(|c| c.input.get("_parse_error").is_some())
            {
                let names: Vec<&str> = tool_calls
                    .iter()
                    .filter_map(|c| {
                        c.input
                            .get("_parse_error")
                            .is_some()
                            .then_some(c.name.as_str())
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
                continue;
            }

            // If no tool calls, we're done.  If the provider set Observe mode,
            // tool calls in the stream are informational only — the provider
            // already executed them internally (e.g. Claude Code CLI, Codex).
            // We display them to the user but don't re-execute, and break to
            // avoid an infinite loop re-feeding already-handled tool_use blocks.
            if tool_calls.is_empty() || tool_mode == crate::llm::ToolMode::Observe {
                if let Some(text) = assistant_msg.last_text() {
                    final_text = text.to_string();
                } else if any_text_streamed {
                    // Retries were exhausted or disabled and this final
                    // iteration came back empty, but the user already saw text
                    // from an earlier iteration.  Surface that text as the
                    // return value so callers (subagents, controllers) don't
                    // receive an empty string.
                    tracing::warn!(
                        "LLM returned no text on final iteration — reusing last \
                         streamed text as the return value"
                    );
                    final_text = last_streamed_text.clone();
                } else {
                    tracing::warn!("LLM returned no text and no tool calls — sending fallback");
                    let fallback = "I wasn't able to generate a response. Please try again.";
                    output.text_delta(fallback)?;
                    final_text = fallback.to_string();
                }
                if !continuation_prefix.is_empty() {
                    final_text = format!("{continuation_prefix}{final_text}");
                }
                self.conversation.messages.push(assistant_msg);
                output.flush()?;
                break;
            }

            self.conversation.messages.push(assistant_msg);
            self.execute_tool_calls(&tool_calls, output).await?;
            self.admit_pending_user_messages(output).await?;
            self.limiter.reset_turn();

            self.maybe_inject_budget_warning(iteration, output);

            if iteration == self.max_iterations - 1 {
                tracing::warn!(
                    max = self.max_iterations,
                    "agent hit maximum iterations — requesting summary"
                );
                hit_max_iterations = true;
            }
        }

        if hit_max_iterations {
            final_text = self
                .summarize_on_max_iterations(&skill_fragments, output)
                .await?;
            if !continuation_prefix.is_empty() {
                final_text = format!("{continuation_prefix}{final_text}");
            }
        }

        output.flush()?;
        Ok(final_text)
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

        let client = match self.client.access() {
            Ok(c) => c,
            Err(e) => return StreamResult::Error(e),
        };

        let err = match client
            .stream(
                &self.conversation.messages,
                &self.system_prompt,
                skill_fragments,
                tools_for_llm,
                &self.config,
            )
            .await
        {
            Ok(s) => return StreamResult::Response(s),
            Err(e) => e,
        };

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
        StreamResult::Recovered
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

/// Exponential backoff with up-to-half jitter: 1s * 2^attempt + rand(0..base/2+1).
/// Shared between the stream-error and empty-response retry paths so both
/// always have the same shape and the constants live in one place.
fn compute_backoff_ms(attempt: usize) -> u64 {
    let base_ms = 1000u64.saturating_mul(2u64.saturating_pow(attempt as u32));
    let jitter_ms = rand::random::<u64>() % (base_ms / 2 + 1);
    base_ms + jitter_ms
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
            assert!((lo..=hi).contains(&v), "attempt {n}: {v} outside [{lo},{hi}]");
        }
    }
}
