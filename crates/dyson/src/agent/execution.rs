// ===========================================================================
// Tool execution engine — sandbox-gated tool dispatch with hooks and timing.
//
// Extracted from agent/mod.rs to keep the core agent loop focused on
// iteration logic.  This module handles:
//   - Rate-limit checking per call
//   - Dependency analysis for parallel/sequential execution
//   - Sandbox gating (Allow/Deny/Redirect)
//   - Pre/post tool hooks
//   - Timing and structured logging
//   - Result formatting and file dispatch
// ===========================================================================

use crate::controller::Output;
use crate::error::{DysonError, Result};
use crate::llm::ToolDefinition;
use crate::message::Message;
use crate::sandbox::SandboxDecision;
use crate::tool::ToolOutput;

use super::dependency_analyzer::{DependencyAnalyzer, ExecutionPhase};
use super::stream_handler::ToolCall;
use super::Agent;

impl Agent {
    /// Check rate limits, analyze dependencies, and execute tool calls in phases.
    pub(super) async fn execute_tool_calls(
        &mut self,
        tool_calls: &[ToolCall],
        output: &mut dyn Output,
    ) -> Result<()> {
        let mut limited_calls: Vec<usize> = Vec::with_capacity(tool_calls.len());
        for (i, call) in tool_calls.iter().enumerate() {
            if let Err(e) = self.limiter.check(&call.name) {
                tracing::warn!(tool = call.name, error = %e, "tool call rate-limited");
                self.conversation.messages
                    .push(Message::tool_result(&call.id, &e.to_string(), true));
            } else {
                limited_calls.push(i);
            }
        }

        let allowed_calls: Vec<&ToolCall> =
            limited_calls.iter().map(|&i| &tool_calls[i]).collect();

        if allowed_calls.is_empty() {
            return Ok(());
        }

        let phases = DependencyAnalyzer::analyze(&allowed_calls);

        for phase in phases {
            match phase {
                ExecutionPhase::Parallel(indices) => {
                    let futs: Vec<_> = indices
                        .iter()
                        .map(|&idx| self.execute_tool_call_timed(allowed_calls[idx]))
                        .collect();
                    let results = futures_util::future::join_all(futs).await;
                    for (&idx, result) in indices.iter().zip(results) {
                        self.handle_tool_result(allowed_calls[idx], result, output)?;
                    }
                }
                ExecutionPhase::Sequential(indices) => {
                    for &idx in &indices {
                        let result = self.execute_tool_call_timed(allowed_calls[idx]).await;
                        self.handle_tool_result(allowed_calls[idx], result, output)?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Make one final tool-free LLM call to summarize progress.
    pub(super) async fn summarize_on_max_iterations(
        &mut self,
        skill_fragments: &str,
        output: &mut dyn Output,
    ) -> Result<String> {
        self.conversation.messages.push(Message::user(
            "You have reached the maximum number of iterations and must stop now. \
             Please provide a brief summary of:\n\
             1. What you have accomplished so far\n\
             2. What still needs to be done\n\
             3. Any relevant partial results\n\n\
             Do NOT call any tools. Just summarize.",
        ));

        let empty_tools: &[ToolDefinition] = &[];
        match self
            .client
            .access()?
            .stream(
                &self.conversation.messages,
                &self.system_prompt,
                skill_fragments,
                empty_tools,
                &self.config,
            )
            .await
        {
            Ok(response) => {
                let (assistant_msg, _tool_calls, _output_tokens) =
                    super::stream_handler::process_stream(response.stream, output).await?;
                let text = assistant_msg
                    .last_text()
                    .unwrap_or_default()
                    .to_string();
                self.conversation.messages.push(assistant_msg);
                Ok(text)
            }
            Err(e) => {
                tracing::warn!(error = %e, "summary LLM call failed — falling back to error");
                output.error(&DysonError::Llm(format!(
                    "Reached maximum iterations ({}) — stopping",
                    self.max_iterations
                )))?;
                Ok(String::new())
            }
        }
    }

    /// Check if an LLM error is retryable (rate limit, overloaded, network).
    pub(super) fn is_retryable(err: &DysonError) -> bool {
        matches!(
            err,
            DysonError::LlmRateLimit(_)
                | DysonError::LlmOverloaded(_)
                | DysonError::Http(_)
        )
    }

    /// Process a tool execution result: render to output, format for the LLM,
    /// send attached files, and append the tool_result message to history.
    fn handle_tool_result(
        &mut self,
        call: &ToolCall,
        result: Result<(ToolOutput, std::time::Duration)>,
        output: &mut dyn Output,
    ) -> Result<()> {
        let tool_result_msg = match result {
            Ok((ref tool_output, duration)) => {
                output.tool_result(tool_output)?;

                // Send any attached files to the user via the controller.
                for file_path in &tool_output.files {
                    if let Err(e) = output.send_file(file_path) {
                        tracing::warn!(
                            path = %file_path.display(),
                            error = %e,
                            "failed to send file"
                        );
                    }
                }

                // Forward any progress checkpoints emitted by the tool.
                // This is the side-channel used by the `swarm_checkpoint`
                // builtin to push progress updates to the swarm hub.
                // Outside of the swarm controller the default
                // `Output::checkpoint` impl drops them.
                for cp in &tool_output.checkpoints {
                    if let Err(e) = output.checkpoint(cp) {
                        tracing::warn!(error = %e, "failed to deliver checkpoint");
                    }
                }

                // Format the result for the LLM with the actual execution duration.
                let formatted = self.formatter.format(call, tool_output, duration);
                let content = formatted.to_llm_message();
                Message::tool_result(&call.id, &content, tool_output.is_error)
            }
            Err(ref e) => Message::tool_result(&call.id, &e.to_string(), true),
        };

        self.conversation.messages.push(tool_result_msg);
        Ok(())
    }

    /// Execute a single tool call with timing and structured logging.
    ///
    /// This is the concurrent-safe entry point — it doesn't touch `output`
    /// (which is `&mut` and can't be shared across futures).  The caller
    /// handles output rendering after all futures resolve.
    ///
    /// Returns the tool output paired with the wall-clock execution duration
    /// so the caller can thread it to the result formatter.
    async fn execute_tool_call_timed(&self, call: &ToolCall) -> Result<(ToolOutput, std::time::Duration)> {
        if tracing::enabled!(tracing::Level::INFO) {
            let input_str = call.input.to_string();
            let input_preview = &input_str[..input_str.len().min(500)];
            tracing::info!(
                tool = call.name,
                id = call.id,
                input = input_preview,
                "executing tool call"
            );
        }
        // -- Pre-tool hooks --
        let effective_call;
        let call = if !self.tool_hooks.is_empty() {
            use crate::tool_hooks::{ToolHookEvent, HookDecision, dispatch_hooks};
            let decision = dispatch_hooks(
                &self.tool_hooks,
                &ToolHookEvent::PreToolUse { call },
            );
            match decision {
                HookDecision::Block { reason } => {
                    tracing::info!(tool = call.name, reason = reason, "tool call blocked by hook");
                    return Ok((ToolOutput::error(format!("Blocked by hook: {reason}")), std::time::Duration::ZERO));
                }
                HookDecision::Modify { input } => {
                    effective_call = ToolCall::new(&call.name, input);
                    &effective_call
                }
                HookDecision::Allow => call,
            }
        } else {
            call
        };

        let tool_start = std::time::Instant::now();
        let result = self.execute_tool_call(call).await;
        let duration = tool_start.elapsed();
        let tool_ms = duration.as_millis();

        // -- Post-tool hooks --
        if !self.tool_hooks.is_empty() {
            use crate::tool_hooks::{ToolHookEvent, dispatch_hooks};
            match &result {
                Ok(output) => {
                    dispatch_hooks(
                        &self.tool_hooks,
                        &ToolHookEvent::PostToolUse { output, duration },
                    );
                }
                Err(e) => {
                    let err = crate::error::DysonError::Tool {
                        tool: call.name.clone(),
                        message: e.to_string(),
                    };
                    dispatch_hooks(
                        &self.tool_hooks,
                        &ToolHookEvent::PostToolUseFailure { error: &err },
                    );
                }
            }
        }

        match &result {
            Ok(out) => {
                let output_preview = &out.content[..out.content.len().min(500)];
                tracing::info!(
                    tool = call.name,
                    duration_ms = tool_ms,
                    is_error = out.is_error,
                    output_len = out.content.len(),
                    output_preview = output_preview,
                    "tool call finished"
                );
            }
            Err(e) => tracing::error!(
                tool = call.name,
                duration_ms = tool_ms,
                error = %e,
                "tool call failed"
            ),
        }
        result.map(|out| (out, duration))
    }

    /// Notify the owning skill that one of its tools was executed.
    ///
    /// Errors are logged but don't interrupt the agent loop — after_tool
    /// is observational, not control flow.
    async fn notify_after_tool(&self, tool_name: &str, output: &ToolOutput) {
        if let Some(skill_idx) = self.tool_registry.skill_index(tool_name)
            && let Err(e) = self.skills[skill_idx].after_tool(tool_name, output).await
        {
            tracing::warn!(
                skill = self.skills[skill_idx].name(),
                tool = tool_name,
                error = %e,
                "skill after_tool hook failed"
            );
        }
    }

    /// Execute a single tool call, routing through the sandbox.
    ///
    /// ## Flow
    ///
    /// 1. `sandbox.check()` → Allow / Deny / Redirect
    /// 2. On Allow: look up tool → `tool.run()` → `sandbox.after()`
    /// 3. On Deny: return error ToolOutput
    /// 4. On Redirect: look up redirected tool → run it → `sandbox.after()`
    async fn execute_tool_call(&self, call: &ToolCall) -> Result<ToolOutput> {
        // -- Ask the sandbox --
        //
        // Sandbox and tool-lookup errors are converted to error ToolOutputs
        // so they flow back to the LLM as tool_result messages instead of
        // crashing the agent loop.  A sandbox failure is not a fatal error —
        // the LLM should learn the tool was rejected and try something else.
        let decision = match self
            .sandbox
            .check(&call.name, &call.input, &self.tool_context)
            .await
        {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(tool = call.name, error = %e, "sandbox check failed");
                return Ok(ToolOutput::error(format!("Sandbox error: {e}")));
            }
        };

        match decision {
            SandboxDecision::Allow { input } => {
                // Look up the tool.
                let Some(tool) = self.tool_registry.get(&call.name) else {
                    tracing::warn!(tool = call.name, "unknown tool");
                    return Ok(ToolOutput::error(format!(
                        "Unknown tool '{}'",
                        call.name
                    )));
                };

                // Execute the tool.
                let mut tool_output = match tool.run(&input, &self.tool_context).await {
                    Ok(out) => out,
                    Err(e) => ToolOutput::error(e.to_string()),
                };

                // Post-process through the sandbox.
                if let Err(e) = self
                    .sandbox
                    .after(&call.name, &input, &mut tool_output)
                    .await
                {
                    tracing::warn!(tool = call.name, error = %e, "sandbox after-hook failed");
                }

                // Notify the owning skill.
                self.notify_after_tool(&call.name, &tool_output).await;

                Ok(tool_output)
            }

            SandboxDecision::Deny { reason } => {
                tracing::info!(
                    tool = call.name,
                    reason = reason,
                    "tool call denied by sandbox"
                );
                let tool_output = ToolOutput::error(format!("Denied by sandbox: {reason}"));
                Ok(tool_output)
            }

            SandboxDecision::Redirect { tool_name, input } => {
                tracing::info!(
                    original = call.name,
                    redirected = tool_name,
                    "tool call redirected by sandbox"
                );

                // Look up the redirected tool.
                let Some(tool) = self.tool_registry.get(&tool_name) else {
                    tracing::warn!(tool = tool_name, "sandbox redirected to unknown tool");
                    return Ok(ToolOutput::error(format!(
                        "Sandbox redirected to unknown tool '{tool_name}'"
                    )));
                };

                // Execute the redirected tool.
                let mut tool_output = match tool.run(&input, &self.tool_context).await {
                    Ok(out) => out,
                    Err(e) => ToolOutput::error(e.to_string()),
                };

                // Post-process.
                if let Err(e) = self
                    .sandbox
                    .after(&tool_name, &input, &mut tool_output)
                    .await
                {
                    tracing::warn!(tool = tool_name, error = %e, "sandbox after-hook failed");
                }

                // Notify the owning skill (using the redirected tool name).
                self.notify_after_tool(&tool_name, &tool_output).await;

                Ok(tool_output)
            }
        }
    }
}
