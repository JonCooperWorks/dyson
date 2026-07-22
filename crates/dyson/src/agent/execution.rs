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
use crate::tool::{ToolContext, ToolExecutionPlan, ToolOutput};

use super::Agent;
use super::dependency_analyzer::{DependencyAnalyzer, ExecutionPhase};
use super::stream_handler::ToolCall;

impl Agent {
    /// Execute one registered tool directly, without an LLM turn.
    ///
    /// Used by controller-owned slash commands for executable local skills.
    /// The call still flows through the normal sandbox, hooks, timeout/cancel
    /// context, and skill after_tool hook; the caller decides how to render
    /// and persist the user-visible response.
    pub async fn execute_tool_direct(
        &mut self,
        tool_name: &str,
        input: serde_json::Value,
    ) -> Result<ToolOutput> {
        self.begin_run_protocol();
        let result = if let Err(e) = self.limiter.check(tool_name) {
            Ok(ToolOutput::error(e.to_string()))
        } else {
            let call = ToolCall::new(tool_name, input);
            self.execute_tool_call_timed(&call)
                .await
                .map(|(output, _)| output)
        };
        self.finish_run_protocol(&result);
        result
    }

    /// Check rate limits, analyze dependencies, and execute tool calls in phases.
    pub(super) async fn execute_tool_calls(
        &mut self,
        tool_calls: &[ToolCall],
        output: &mut dyn Output,
    ) -> Result<()> {
        let mut limited_calls: Vec<usize> = Vec::with_capacity(tool_calls.len());
        let mut persisted_rate_limit_result = false;
        for (i, call) in tool_calls.iter().enumerate() {
            if let Err(e) = self.limiter.check(&call.name) {
                tracing::warn!(tool = call.name, error = %e, "tool call rate-limited");
                self.conversation.messages.push(Message::tool_result(
                    &call.id,
                    &e.to_string(),
                    true,
                ));
                persisted_rate_limit_result = true;
            } else {
                limited_calls.push(i);
            }
        }
        if persisted_rate_limit_result {
            self.persist();
        }

        let allowed_calls: Vec<&ToolCall> = limited_calls.iter().map(|&i| &tool_calls[i]).collect();

        if allowed_calls.is_empty() {
            return Ok(());
        }

        let plans: Vec<_> = allowed_calls
            .iter()
            .map(|call| {
                self.tool_registry
                    .get(&call.name)
                    .map_or_else(crate::tool::ToolExecutionPlan::exclusive, |tool| {
                        tool.execution_plan(&call.input, &self.tool_context)
                    })
            })
            .collect();
        let phases = DependencyAnalyzer::analyze_plans(&plans);

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
                let (assistant_msg, _tool_calls, _output_tokens, _stop_reason) =
                    super::stream_handler::process_stream(response.stream, output).await?;
                let text = assistant_msg.last_text().unwrap_or_default().to_string();
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
                // Side-channel: the default `Output::checkpoint` impl drops
                // them; controllers that want progress reporting override it.
                for cp in &tool_output.checkpoints {
                    if let Err(e) = output.checkpoint(cp) {
                        tracing::warn!(error = %e, "failed to deliver checkpoint");
                    }
                }

                // Forward any artefacts (e.g. security-review reports)
                // emitted by the tool.  Side-channel — the LLM never
                // sees these; the HTTP controller renders them in the
                // Artefacts tab.  Other controllers drop them.
                for artefact in &tool_output.artefacts {
                    if let Err(e) = output.send_artefact(artefact) {
                        tracing::warn!(error = %e, "failed to deliver artefact");
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
        self.persist();
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
    async fn execute_tool_call_timed(
        &self,
        call: &ToolCall,
    ) -> Result<(ToolOutput, std::time::Duration)> {
        if tracing::enabled!(tracing::Level::INFO) {
            let input_str = call.input.to_string();
            let input_preview = super::result_formatter::preview(&input_str, 500);
            tracing::info!(
                tool = call.name,
                id = call.id,
                input = input_preview,
                "executing tool call"
            );
        }
        // Liveness signal for the Activity tab's stale-cleanup.  A
        // tool call means this chat's Running entries are still doing
        // work; reset their idle counters so they don't get reaped.
        if let Some(activity) = &self.tool_context.activity {
            activity.touch();
        }
        // -- Pre-tool hooks --
        let effective_call;
        let call = if !self.tool_hooks.is_empty() {
            use crate::tool_hooks::{HookDecision, ToolHookEvent, dispatch_hooks};
            let decision = dispatch_hooks(&self.tool_hooks, &ToolHookEvent::PreToolUse { call });
            match decision {
                HookDecision::Block { reason } => {
                    tracing::info!(
                        tool = call.name,
                        reason = reason,
                        "tool call blocked by hook"
                    );
                    return Ok((
                        ToolOutput::error(format!("Blocked by hook: {reason}")),
                        std::time::Duration::ZERO,
                    ));
                }
                HookDecision::Modify { input } => {
                    // Input rewriting must not mint a second protocol id: the
                    // provider is waiting for a result tied to the original
                    // tool_use id, and journal/UI correlation uses it too.
                    effective_call = ToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        input,
                    };
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
                let output_preview = super::result_formatter::preview(&out.content, 500);
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
        use sha2::Digest as _;
        let input_bytes = serde_json::to_vec(&call.input).unwrap_or_default();
        let input_sha256 = format!("{:x}", sha2::Sha256::digest(input_bytes));
        self.emit_run_event(super::protocol::RunEventKind::ToolRequested {
            tool_use_id: call.id.clone(),
            tool_name: call.name.clone(),
            input_sha256,
        });
        // Per-call clone of the tool context so each tool sees its own
        // `tool_use_id`.  Cloning is cheap (Arcs and small primitives)
        // and is the only safe way to stamp per-call state when the
        // dispatch path is `&self` and parallel-friendly.  Subagent
        // tools propagate this id into `ChildSpawn.parent_tool_id` so
        // the inner agent's `CaptureOutput` can tag every nested SSE
        // event with its owning subagent box.
        let mut ctx = self.tool_context.clone();
        ctx.tool_use_id = Some(call.id.clone());
        let scheduled_plan = self
            .tool_registry
            .get(&call.name)
            .map_or_else(ToolExecutionPlan::exclusive, |tool| {
                tool.execution_plan(&call.input, &ctx)
            });
        // -- Ask the sandbox --
        //
        // Sandbox and tool-lookup errors are converted to error ToolOutputs
        // so they flow back to the LLM as tool_result messages instead of
        // crashing the agent loop.  A sandbox failure is not a fatal error —
        // the LLM should learn the tool was rejected and try something else.
        let decision = match self.sandbox.check(&call.name, &call.input, &ctx).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(tool = call.name, error = %e, "sandbox check failed");
                return Ok(ToolOutput::error(format!("Sandbox error: {e}")));
            }
        };

        match decision {
            SandboxDecision::Allow { input } => {
                self.run_named_tool(&call.name, &input, &ctx, &scheduled_plan)
                    .await
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
                // The redirected call keeps the original `tool_use_id` — the
                // sandbox didn't change which message id the LLM is waiting on.
                self.run_named_tool(&tool_name, &input, &ctx, &scheduled_plan)
                    .await
            }
        }
    }

    /// Look up `name`, run it with `input`, post-process through
    /// `sandbox.after`, and notify the owning skill.  Shared by the
    /// Allow and Redirect sandbox arms so post-processing stays identical.
    async fn run_named_tool(
        &self,
        name: &str,
        input: &serde_json::Value,
        ctx: &ToolContext,
        scheduled_plan: &ToolExecutionPlan,
    ) -> Result<ToolOutput> {
        let Some(tool) = self.tool_registry.get(name) else {
            tracing::warn!(tool = name, "unknown tool");
            return Ok(ToolOutput::error(format!("Unknown tool '{name}'")));
        };

        if let Err(error) = crate::tool::validate_tool_input(&tool.input_schema(), input) {
            tracing::warn!(tool = name, error, "tool input failed schema validation");
            return Ok(ToolOutput::error(format!(
                "Invalid input for tool '{name}': {error}"
            )));
        }

        let plan = tool.execution_plan(input, ctx);
        if !crate::tool::execution_plan_covers(scheduled_plan, &plan) {
            tracing::warn!(
                tool = name,
                "sandbox rewrite expanded the scheduled execution plan"
            );
            return Ok(ToolOutput::error(format!(
                "Policy rewrite for '{name}' changed its resource or side-effect footprint; execution was withheld to prevent an unsafe scheduling race"
            )));
        }
        self.emit_run_event(super::protocol::RunEventKind::ToolAuthorized {
            tool_use_id: ctx.tool_use_id.clone().unwrap_or_default(),
            effective_tool_name: name.to_string(),
            idempotency: plan.idempotency,
            timeout_ms: plan.timeout_ms,
        });
        let idempotency_key = format!(
            "{}:{}",
            self.active_run_id.0,
            ctx.tool_use_id.as_deref().unwrap_or("direct")
        );
        self.emit_run_event(super::protocol::RunEventKind::ToolStarted {
            tool_use_id: ctx.tool_use_id.clone().unwrap_or_default(),
            effective_tool_name: name.to_string(),
            idempotency_key,
        });

        let started = std::time::Instant::now();
        let mut tool_output = match tokio::time::timeout(
            std::time::Duration::from_millis(plan.timeout_ms.max(1)),
            tool.run(input, ctx),
        )
        .await
        {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => ToolOutput::error(e.to_string()),
            Err(_) => {
                self.emit_run_event(super::protocol::RunEventKind::ToolOutcomeUnknown {
                    tool_use_id: ctx.tool_use_id.clone().unwrap_or_default(),
                    effective_tool_name: name.to_string(),
                    reason: format!("execution exceeded {} ms deadline", plan.timeout_ms),
                });
                return Ok(ToolOutput::error(format!(
                    "Tool '{name}' exceeded its {} ms execution deadline; side-effect outcome is unknown and the runtime will not retry it automatically",
                    plan.timeout_ms
                )));
            }
        };

        if let Err(e) = self.sandbox.after(name, input, &mut tool_output).await {
            tracing::warn!(tool = name, error = %e, "sandbox after-hook failed");
            // Post-processing commonly performs redaction.  Returning the raw
            // output on failure would turn an observability problem into a
            // secret-disclosure path, so fail closed.
            tool_output = ToolOutput::error(format!(
                "Sandbox post-processing failed for '{name}'; output withheld"
            ));
        }

        self.notify_after_tool(name, &tool_output).await;

        self.emit_run_event(super::protocol::RunEventKind::ToolFinished {
            tool_use_id: ctx.tool_use_id.clone().unwrap_or_default(),
            effective_tool_name: name.to_string(),
            is_error: tool_output.is_error,
            duration_ms: started.elapsed().as_millis() as u64,
        });

        Ok(tool_output)
    }
}
