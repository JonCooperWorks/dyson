// ===========================================================================
// Agent — the streaming tool-use loop at the heart of Dyson.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements the core agent loop: send messages to the LLM, stream the
//   response, detect tool calls, execute them through the sandbox, feed
//   results back, repeat.  This is the star in the cage — everything else
//   in Dyson exists to feed this loop.
//
// Module layout:
//   mod.rs             — Agent struct and the loop (this file)
//   stream_handler.rs  — Processes StreamEvents into Messages and ToolCalls
//
// The loop in pseudocode:
//
//   messages = [user_input]
//   for iteration in 0..max_iterations:
//       stream = llm.stream(messages, system_prompt, tools, config)
//       (assistant_msg, tool_calls) = process_stream(stream, output)
//       messages.push(assistant_msg)
//
//       if tool_calls.is_empty():
//           break  // LLM is done, no more tools to run
//
//       for call in tool_calls:
//           decision = sandbox.check(call.name, call.input, ctx)
//           match decision:
//               Allow { input }  → result = tool.run(input, ctx)
//                                   sandbox.after(call.name, &input, &mut result)
//               Deny { reason }  → result = ToolOutput::error(reason)
//               Redirect { .. }  → result = other_tool.run(...)
//                                   sandbox.after(...)
//           messages.push(tool_result(call.id, result))
//
//       // loop — LLM sees tool results on next iteration
//
// Architecture:
//
//   Agent owns:
//     ┌─────────────────────────────────────────────────┐
//     │  client:  Box<dyn LlmClient>                    │
//     │  sandbox: Box<dyn Sandbox>     ← gates all calls│
//     │  skills:  Vec<Box<dyn Skill>>                   │
//     │  tools:   HashMap<name, Arc<dyn Tool>>          │
//     │  tool_definitions: Vec<ToolDefinition>          │
//     │  system_prompt: String                          │
//     │  config: CompletionConfig                       │
//     │  messages: Vec<Message>        ← conversation   │
//     │  max_iterations: usize                          │
//     └─────────────────────────────────────────────────┘
//
// Why does Agent own both skills AND a flat tools map?
//   Skills own tools (for lifecycle management), but the agent needs O(1)
//   lookup by tool name when dispatching calls.  The flat HashMap provides
//   that.  Both hold Arc<dyn Tool> to the same underlying objects — no
//   duplication, just shared references.
// ===========================================================================

pub mod stream_handler;

use std::collections::HashMap;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::config::AgentSettings;
use crate::error::{DysonError, Result};
use crate::llm::{CompletionConfig, LlmClient, ToolDefinition};
use crate::message::Message;
use crate::sandbox::{Sandbox, SandboxDecision};
use crate::skill::Skill;
use crate::tool::{Tool, ToolContext, ToolOutput};
use crate::controller::Output;

use self::stream_handler::ToolCall;

// ---------------------------------------------------------------------------
// TokenBudget — tracks and limits token usage across the agent session.
// ---------------------------------------------------------------------------

/// Token usage tracking and optional budget enforcement.
///
/// Hooks into the agent loop via `process_stream`'s reported `output_tokens`.
/// When a `max_output_tokens` budget is set, the agent loop stops with an
/// error once the cumulative output tokens exceed the limit.
///
/// ## Usage
///
/// ```ignore
/// let mut budget = TokenBudget::default();
/// budget.max_output_tokens = Some(100_000); // cap at 100k output tokens
/// ```
#[derive(Debug, Clone, Default)]
pub struct TokenBudget {
    /// Maximum cumulative output tokens before the agent refuses to continue.
    /// `None` = unlimited (default).
    pub max_output_tokens: Option<usize>,

    /// Cumulative output tokens used across all turns in this session.
    pub output_tokens_used: usize,

    /// Cumulative input tokens used across all turns in this session.
    pub input_tokens_used: usize,

    /// Number of LLM calls made in this session (across all `run()` calls).
    pub llm_calls: usize,
}

impl TokenBudget {
    /// Record tokens from a completed LLM turn.
    ///
    /// Returns `Err` if the budget is exceeded after recording.
    pub fn record(&mut self, output_tokens: usize) -> Result<()> {
        self.output_tokens_used += output_tokens;
        self.llm_calls += 1;
        if let Some(max) = self.max_output_tokens && self.output_tokens_used > max {
            return Err(DysonError::Llm(format!(
                "token budget exceeded: {}/{max} output tokens used",
                self.output_tokens_used,
            )));
        }
        Ok(())
    }

    /// Check if there's budget remaining (without recording).
    pub fn has_budget(&self) -> bool {
        match self.max_output_tokens {
            Some(max) => self.output_tokens_used < max,
            None => true,
        }
    }

    /// Record input tokens from a completed LLM turn (informational only).
    pub fn record_input(&mut self, input_tokens: usize) {
        self.input_tokens_used += input_tokens;
    }

    /// Reset the budget counters (e.g., on `clear()`).
    pub fn reset(&mut self) {
        self.output_tokens_used = 0;
        self.input_tokens_used = 0;
        self.llm_calls = 0;
    }
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// The streaming tool-use agent — Dyson's core runtime.
///
/// Created once at startup, then `run()` is called for each user message.
/// Conversation history (`messages`) persists across calls for multi-turn
/// conversations.
pub struct Agent {
    /// LLM client for streaming completions.
    client: Box<dyn LlmClient>,

    /// Sandbox that gates all tool execution.
    sandbox: Box<dyn Sandbox>,

    /// Loaded skills (retained for lifecycle: before_turn, after_tool, on_unload).
    skills: Vec<Box<dyn Skill>>,

    /// Flat tool lookup map: tool_name → Arc<dyn Tool>.
    ///
    /// Built at construction by flattening all skills' tools.  Shared
    /// ownership (Arc) with the skills — no cloning of tool implementations.
    tools: HashMap<String, Arc<dyn Tool>>,

    /// Reverse index: tool_name → skill index in `self.skills`.
    ///
    /// Used to dispatch `after_tool()` to the owning skill.
    tool_to_skill: HashMap<String, usize>,

    /// Tool definitions sent to the LLM so it knows what tools are available.
    tool_definitions: Vec<ToolDefinition>,

    /// Composed system prompt: base prompt + all skill prompt fragments.
    system_prompt: String,

    /// LLM configuration (model, max_tokens, temperature).
    config: CompletionConfig,

    /// Maximum LLM turns per `run()` call.
    max_iterations: usize,

    /// Maximum retries on transient LLM errors (HTTP 429, 529, network).
    max_retries: usize,

    /// Conversation history.  Persists across `run()` calls.
    messages: Vec<Message>,

    /// Shared tool context (working dir, env, cancellation).
    tool_context: ToolContext,

    /// Number of user turns processed (for nudge timing).
    turn_count: usize,

    /// Inject a memory maintenance nudge every N turns.  0 = disabled.
    nudge_interval: usize,

    /// Token usage tracking and optional budget enforcement.
    pub token_budget: TokenBudget,
}

impl Agent {
    /// Construct a new agent from its components.
    ///
    /// This flattens all skills' tools into the agent's lookup map and
    /// composes the system prompt from the base prompt + skill fragments.
    ///
    /// ## Panics
    ///
    /// Does not panic.  Duplicate tool names are handled by last-writer-wins
    /// (later skills override earlier ones), with a warning logged.
    pub fn new(
        client: Box<dyn LlmClient>,
        sandbox: Box<dyn Sandbox>,
        skills: Vec<Box<dyn Skill>>,
        settings: &AgentSettings,
        workspace: Option<std::sync::Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>>>,
        nudge_interval: usize,
    ) -> Result<Self> {
        // -- Flatten tools from all skills --
        let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        let mut tool_to_skill: HashMap<String, usize> = HashMap::new();
        let mut tool_definitions: Vec<ToolDefinition> = Vec::new();

        for (skill_idx, skill) in skills.iter().enumerate() {
            for tool in skill.tools() {
                let name = tool.name().to_string();

                if tools.contains_key(&name) {
                    tracing::warn!(
                        tool = name,
                        skill = skill.name(),
                        "duplicate tool name — overriding previous registration"
                    );
                }

                tool_definitions.push(ToolDefinition {
                    name: name.clone(),
                    description: tool.description().to_string(),
                    input_schema: tool.input_schema(),
                    agent_only: tool.agent_only(),
                });

                tools.insert(name.clone(), Arc::clone(tool));
                tool_to_skill.insert(name, skill_idx);
            }
        }

        tracing::info!(
            tool_count = tools.len(),
            skill_count = skills.len(),
            "agent initialized"
        );

        // -- Compose system prompt --
        //
        // Start with the base prompt, then append each skill's fragment.
        // Skills contribute context like "You have access to bash..." or
        // "The following MCP tools are available...".
        let mut system_prompt = settings.system_prompt.clone();

        // Inject model/provider info so the model can answer "what model
        // are you running?" accurately instead of guessing from its
        // training data or workspace identity files.
        system_prompt.push_str(&format!(
            "\n\nYou are running on model '{}' via the {:?} provider.",
            settings.model, settings.provider,
        ));

        for skill in &skills {
            if let Some(fragment) = skill.system_prompt() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(fragment);
            }
        }

        // -- Build completion config --
        let config = CompletionConfig {
            model: settings.model.clone(),
            max_tokens: settings.max_tokens,
            temperature: None, // use provider default
        };

        // Use the workspace's programs directory as the working directory
        // for coding tools.  This gives the agent a dedicated place to create
        // and manage projects (e.g. ~/.dyson/programs/).  Falls back to the
        // process CWD when no workspace is configured.
        let working_dir = workspace
            .as_ref()
            .and_then(|ws| {
                let guard = ws.try_read().ok()?;
                guard.programs_dir()
            })
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        let mut tool_context = ToolContext {
            working_dir,
            env: HashMap::new(),
            cancellation: CancellationToken::new(),
            workspace: None,
        };
        tool_context.workspace = workspace;

        Ok(Self {
            client,
            sandbox,
            skills,
            tools,
            tool_to_skill,
            tool_definitions,
            system_prompt,
            config,
            max_iterations: settings.max_iterations,
            max_retries: 3,
            messages: Vec::new(),
            tool_context,
            turn_count: 0,
            nudge_interval,
            token_budget: TokenBudget::default(),
        })
    }

    /// Clear conversation history, starting fresh.
    ///
    /// Resets the agent to a blank slate by dropping all accumulated
    /// messages.  This is the in-memory half of the `/clear` flow — the
    /// caller (e.g. the Telegram controller) is responsible for also
    /// rotating persisted history via [`ChatHistory::rotate`] so old
    /// conversations are archived rather than lost.
    ///
    /// ## Full `/clear` flow (Telegram)
    ///
    /// 1. Remove the [`Agent`] from the in-memory map (drops all state).
    /// 2. Call [`ChatHistory::rotate`] to rename the on-disk history file
    ///    with a timestamp, preserving it for review or RAG indexing.
    /// 3. Reply "Context cleared." to the user.
    /// 4. The next incoming message creates a fresh `Agent` with an empty
    ///    history.
    pub fn clear(&mut self) {
        self.messages.clear();
    }

    /// Get the current conversation messages (for persistence).
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Replace the conversation history (for restoring from persistence).
    pub fn set_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
    }

    /// Run the agent loop for a single user message.
    ///
    /// Appends the user message to the conversation history, then loops:
    /// stream LLM response → execute tool calls → repeat until done.
    ///
    /// The conversation history persists across calls, so the LLM has
    /// context from previous turns.
    ///
    /// ## Returns
    ///
    /// The final assistant text (the last text content from the last
    /// assistant message without tool calls), or an error if something
    /// went wrong.
    pub async fn run(&mut self, user_input: &str, output: &mut dyn Output) -> Result<String> {
        // Append the user's message to history.
        self.messages.push(Message::user(user_input));
        self.turn_count += 1;

        // Inject a memory maintenance nudge every N turns.
        if self.nudge_interval > 0 && self.turn_count.is_multiple_of(self.nudge_interval) && let Some(ref ws) = self.tool_context.workspace {
            let ws = ws.read().await;
            let nudge = Self::build_nudge_message(&**ws);
            drop(ws);
            self.messages.push(Message::user(&nudge));
        }

        let mut final_text = String::new();

        // -- Collect ephemeral context from skills --
        //
        // Each skill can inject per-turn context (refreshed tokens, timestamps,
        // etc.) via before_turn().  These fragments are appended to the system
        // prompt for this run() call only — they don't persist.
        let mut turn_system_prompt = self.system_prompt.clone();
        for skill in &self.skills {
            match skill.before_turn().await {
                Ok(Some(fragment)) => {
                    turn_system_prompt.push_str("\n\n");
                    turn_system_prompt.push_str(&fragment);
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

        for iteration in 0..self.max_iterations {
            tracing::info!(
                iteration = iteration,
                model = self.config.model,
                messages = self.messages.len(),
                "starting LLM call"
            );

            // -- Stream LLM response (with retry/backoff) --
            //
            // When the provider handles tools internally (e.g., Claude Code),
            // don't send Dyson's tool definitions — the provider has its own.
            // We discover this from `StreamResponse.tool_mode` after the call.
            let response = {
                let mut last_err = None;
                let mut response_opt = None;
                for attempt in 0..=self.max_retries {
                    // Determine tools_for_llm inside the loop so retries
                    // behave identically.  On the first successful response
                    // we learn the tool_mode.
                    let tools_for_llm = self.tool_definitions.as_slice();

                    match self
                        .client
                        .stream(
                            &self.messages,
                            &turn_system_prompt,
                            tools_for_llm,
                            &self.config,
                        )
                        .await
                    {
                        Ok(s) => {
                            response_opt = Some(s);
                            break;
                        }
                        Err(e) if attempt < self.max_retries && Self::is_retryable(&e) => {
                            let base_ms = 1000 * 2u64.pow(attempt as u32);
                            let jitter_ms = rand::random::<u64>() % (base_ms / 2 + 1);
                            let delay_ms = base_ms + jitter_ms;
                            tracing::warn!(
                                attempt = attempt + 1,
                                max = self.max_retries,
                                delay_ms = delay_ms,
                                error = %e,
                                "LLM call failed, retrying"
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                            last_err = Some(e);
                        }
                        Err(e) => return Err(e),
                    }
                }
                response_opt.ok_or_else(|| last_err.unwrap())?
            };

            let tool_mode = response.tool_mode;
            if let Some(input_tokens) = response.input_tokens {
                self.token_budget.record_input(input_tokens);
            }

            tracing::info!("streaming response");

            // -- Process the stream into a message + tool calls --
            let (assistant_msg, tool_calls, output_tokens) =
                stream_handler::process_stream(response.stream, output).await?;

            self.messages.push(assistant_msg.clone());

            // -- Record token usage and check budget --
            if let Err(e) = self.token_budget.record(output_tokens) {
                tracing::warn!(
                    used = self.token_budget.output_tokens_used,
                    "token budget exceeded — stopping agent loop"
                );
                output.error(&e)?;
                break;
            }

            // -- If no tool calls, the LLM is done --
            //
            // Also break when the provider handles tools internally (e.g.,
            // Claude Code).  In that case, ToolUse events in the stream are
            // informational — the provider already executed them.  Without
            // this check, Dyson would try to re-execute every tool call and
            // loop until max_iterations, spawning a new subprocess each time.
            if tool_calls.is_empty() || tool_mode == crate::llm::ToolMode::Observe {
                // Extract the final text from the assistant message.
                for block in &assistant_msg.content {
                    if let crate::message::ContentBlock::Text { text } = block {
                        final_text = text.clone();
                    }
                }
                output.flush()?;
                break;
            }

            // -- Execute tool calls concurrently --
            //
            // Independent tool calls are dispatched in parallel via join_all.
            // Results are collected in order and appended to the conversation
            // so the LLM sees them in the same order it requested them.
            let futures: Vec<_> = tool_calls
                .iter()
                .map(|call| self.execute_tool_call_timed(call))
                .collect();
            let results = futures::future::join_all(futures).await;

            for (call, result) in tool_calls.iter().zip(results) {
                let tool_result_msg = match result {
                    Ok(ref tool_output) => {
                        output.tool_result(tool_output)?;
                        Message::tool_result(&call.id, &tool_output.content, tool_output.is_error)
                    }
                    Err(ref e) => Message::tool_result(&call.id, &e.to_string(), true),
                };

                self.messages.push(tool_result_msg);
            }

            // If we're about to hit max iterations, warn.
            if iteration == self.max_iterations - 1 {
                tracing::warn!(
                    max = self.max_iterations,
                    "agent hit maximum iterations — stopping"
                );
                output.error(&DysonError::Llm(format!(
                    "Reached maximum iterations ({}) — stopping",
                    self.max_iterations
                )))?;
            }
        }

        output.flush()?;
        Ok(final_text)
    }

    /// Build the memory maintenance nudge message.
    fn build_nudge_message(ws: &dyn crate::workspace::Workspace) -> String {
        let memory_usage = ws.get("MEMORY.md")
            .map(|c| c.chars().count())
            .unwrap_or(0);
        let memory_limit = ws.char_limit("MEMORY.md").unwrap_or(0);
        let user_usage = ws.get("USER.md")
            .map(|c| c.chars().count())
            .unwrap_or(0);
        let user_limit = ws.char_limit("USER.md").unwrap_or(0);

        format!(
            "[System: Memory Maintenance] Consider saving important details from this conversation.\n\
             MEMORY.md: {memory_usage}/{memory_limit} chars. USER.md: {user_usage}/{user_limit} chars.\n\
             Use workspace_view/workspace_update. Move overflow to memory/notes/ (searchable via memory_search)."
        )
    }

    /// Check if an LLM error is retryable (rate limit, overloaded, network).
    fn is_retryable(err: &DysonError) -> bool {
        match err {
            DysonError::Llm(msg) => {
                msg.contains("rate limit")
                    || msg.contains("429")
                    || msg.contains("overloaded")
                    || msg.contains("529")
                    || msg.contains("502")
                    || msg.contains("503")
            }
            DysonError::Http(_) => true, // network errors are always retryable
            _ => false,
        }
    }

    /// Execute a single tool call with timing and structured logging.
    ///
    /// This is the concurrent-safe entry point — it doesn't touch `output`
    /// (which is `&mut` and can't be shared across futures).  The caller
    /// handles output rendering after all futures resolve.
    async fn execute_tool_call_timed(&self, call: &ToolCall) -> Result<ToolOutput> {
        tracing::info!(tool = call.name, id = call.id, "executing tool call");
        let tool_start = std::time::Instant::now();
        let result = self.execute_tool_call(call).await;
        let tool_ms = tool_start.elapsed().as_millis();
        match &result {
            Ok(out) => tracing::info!(
                tool = call.name,
                duration_ms = tool_ms,
                is_error = out.is_error,
                "tool call finished"
            ),
            Err(e) => tracing::error!(
                tool = call.name,
                duration_ms = tool_ms,
                error = %e,
                "tool call failed"
            ),
        }
        result
    }

    /// Notify the owning skill that one of its tools was executed.
    ///
    /// Errors are logged but don't interrupt the agent loop — after_tool
    /// is observational, not control flow.
    async fn notify_after_tool(&self, tool_name: &str, output: &ToolOutput) {
        if let Some(&skill_idx) = self.tool_to_skill.get(tool_name)
            && let Err(e) = self.skills[skill_idx].after_tool(tool_name, output).await {
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
    async fn execute_tool_call(
        &self,
        call: &ToolCall,
    ) -> Result<ToolOutput> {
        // -- Ask the sandbox --
        let decision = self
            .sandbox
            .check(&call.name, &call.input, &self.tool_context)
            .await?;

        match decision {
            SandboxDecision::Allow { input } => {
                // Look up the tool.
                let tool = self.tools.get(&call.name).ok_or_else(|| {
                    DysonError::tool(&call.name, "unknown tool")
                })?;

                // Execute the tool.
                let mut tool_output = match tool.run(input.clone(), &self.tool_context).await {
                    Ok(out) => out,
                    Err(e) => ToolOutput::error(e.to_string()),
                };

                // Post-process through the sandbox.
                self.sandbox
                    .after(&call.name, &input, &mut tool_output)
                    .await?;

                // Notify the owning skill.
                self.notify_after_tool(&call.name, &tool_output).await;

                Ok(tool_output)
            }

            SandboxDecision::Deny { reason } => {
                tracing::info!(tool = call.name, reason = reason, "tool call denied by sandbox");
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
                let tool = self.tools.get(&tool_name).ok_or_else(|| {
                    DysonError::tool(
                        &tool_name,
                        format!("sandbox redirected to unknown tool '{tool_name}'"),
                    )
                })?;

                // Execute the redirected tool.
                let mut tool_output = match tool.run(input.clone(), &self.tool_context).await {
                    Ok(out) => out,
                    Err(e) => ToolOutput::error(e.to_string()),
                };

                // Post-process.
                self.sandbox
                    .after(&tool_name, &input, &mut tool_output)
                    .await?;

                // Notify the owning skill (using the redirected tool name).
                self.notify_after_tool(&tool_name, &tool_output).await;

                Ok(tool_output)
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
    use crate::llm::stream::{StopReason, StreamEvent};
    use crate::sandbox::no_sandbox::DangerousNoSandbox;
    use crate::skill::builtin::BuiltinSkill;


    // -----------------------------------------------------------------------
    // Mock LLM client that returns a fixed response.
    // -----------------------------------------------------------------------

    struct MockLlm {
        /// Responses to return, in order.  Each call to `stream()` pops
        /// the first entry.
        responses: std::sync::Mutex<Vec<Vec<StreamEvent>>>,
        /// Simulate a provider that handles tools internally (like Claude Code).
        tool_mode: crate::llm::ToolMode,
    }

    impl MockLlm {
        fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
                tool_mode: crate::llm::ToolMode::Execute,
            }
        }

        fn with_internal_tools(responses: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
                tool_mode: crate::llm::ToolMode::Observe,
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmClient for MockLlm {
        async fn stream(
            &self,
            _messages: &[Message],
            _system: &str,
            _tools: &[ToolDefinition],
            _config: &CompletionConfig,
        ) -> Result<crate::llm::StreamResponse> {
            let events = self
                .responses
                .lock()
                .unwrap()
                .remove(0);
            Ok(crate::llm::StreamResponse {
                stream: Box::pin(tokio_stream::iter(events.into_iter().map(Ok))),
                tool_mode: self.tool_mode,
                input_tokens: None,
            })
        }
    }

    // -----------------------------------------------------------------------
    // Mock output
    // -----------------------------------------------------------------------

    struct MockOutput {
        text: String,
    }

    impl MockOutput {
        fn new() -> Self {
            Self { text: String::new() }
        }
    }

    impl Output for MockOutput {
        fn text_delta(&mut self, text: &str) -> Result<()> {
            self.text.push_str(text);
            Ok(())
        }
        fn tool_use_start(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
        fn tool_use_complete(&mut self) -> Result<()> { Ok(()) }
        fn tool_result(&mut self, _: &ToolOutput) -> Result<()> { Ok(()) }
        fn error(&mut self, _: &DysonError) -> Result<()> { Ok(()) }
        fn flush(&mut self) -> Result<()> { Ok(()) }
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn simple_text_response() {
        let llm = MockLlm::new(vec![vec![
            StreamEvent::TextDelta("Hello!".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ]]);

        let settings = AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        };

        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None))];
        let sandbox: Box<dyn Sandbox> = Box::new(DangerousNoSandbox);
        let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();
        let mut output = MockOutput::new();

        let result = agent.run("hi", &mut output).await.unwrap();
        assert_eq!(result, "Hello!");
        assert_eq!(output.text, "Hello!");
    }

    #[tokio::test]
    async fn tool_call_loop() {
        // First LLM call: request a bash command.
        // Second LLM call: respond with the result.
        let llm = MockLlm::new(vec![
            // Turn 1: LLM calls bash.
            vec![
                StreamEvent::ToolUseStart {
                    id: "call_1".into(),
                    name: "bash".into(),
                },
                StreamEvent::ToolUseComplete {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo test_output"}),
                },
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::ToolUse,
                    output_tokens: None,
                },
            ],
            // Turn 2: LLM responds with text.
            vec![
                StreamEvent::TextDelta("Done.".into()),
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    output_tokens: None,
                },
            ],
        ]);

        let settings = AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        };

        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None))];
        let sandbox: Box<dyn Sandbox> = Box::new(DangerousNoSandbox);
        let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();
        let mut output = MockOutput::new();

        let result = agent.run("run echo test_output", &mut output).await.unwrap();
        assert_eq!(result, "Done.");

        // Conversation should have: user, assistant (tool_use), tool_result, assistant (text)
        assert_eq!(agent.messages.len(), 4);
    }

    #[tokio::test]
    async fn internal_tools_provider_skips_tool_execution() {
        // Simulate a provider like Claude Code that handles tools internally.
        // The stream includes tool events, but the agent loop should NOT try
        // to execute them — it should break after one iteration.
        let llm = MockLlm::with_internal_tools(vec![vec![
            StreamEvent::TextDelta("I'll check. ".into()),
            StreamEvent::ToolUseStart {
                id: "cc_1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "cc_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            StreamEvent::TextDelta("Here are the files.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ]]);

        let settings = AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        };

        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None))];
        let sandbox: Box<dyn Sandbox> = Box::new(DangerousNoSandbox);
        let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();
        let mut output = MockOutput::new();

        let result = agent.run("list files", &mut output).await.unwrap();

        // Should get the final text, NOT an error from trying to execute "bash".
        assert_eq!(result, "Here are the files.");
        // Only 2 messages: user + assistant (no tool_result messages).
        assert_eq!(agent.messages.len(), 2);
    }

    #[test]
    fn nudge_message_contains_usage_info() {
        let ws = crate::workspace::InMemoryWorkspace::new()
            .with_file("MEMORY.md", "some memories here")
            .with_limit("MEMORY.md", 2200)
            .with_file("USER.md", "user info")
            .with_limit("USER.md", 1375);

        let nudge = Agent::build_nudge_message(&ws);
        assert!(nudge.contains("[System: Memory Maintenance]"));
        assert!(nudge.contains("MEMORY.md:"));
        assert!(nudge.contains("/2200 chars"));
        assert!(nudge.contains("USER.md:"));
        assert!(nudge.contains("/1375 chars"));
        assert!(nudge.contains("memory_search"));
    }

    // -----------------------------------------------------------------------
    // TokenBudget tests
    // -----------------------------------------------------------------------

    #[test]
    fn token_budget_unlimited_by_default() {
        let budget = TokenBudget::default();
        assert!(budget.has_budget());
        assert_eq!(budget.output_tokens_used, 0);
        assert_eq!(budget.llm_calls, 0);
    }

    #[test]
    fn token_budget_records_and_enforces() {
        let mut budget = TokenBudget {
            max_output_tokens: Some(100),
            ..TokenBudget::default()
        };

        // Under budget — should succeed.
        assert!(budget.record(50).is_ok());
        assert_eq!(budget.output_tokens_used, 50);
        assert_eq!(budget.llm_calls, 1);
        assert!(budget.has_budget());

        // Still under — should succeed.
        assert!(budget.record(49).is_ok());
        assert_eq!(budget.output_tokens_used, 99);
        assert!(budget.has_budget());

        // Over budget — should fail.
        assert!(budget.record(10).is_err());
        assert_eq!(budget.output_tokens_used, 109);
        assert!(!budget.has_budget());
    }

    #[test]
    fn token_budget_reset() {
        let mut budget = TokenBudget {
            max_output_tokens: Some(100),
            ..TokenBudget::default()
        };
        budget.record(80).unwrap();
        assert_eq!(budget.llm_calls, 1);

        budget.reset();
        assert_eq!(budget.output_tokens_used, 0);
        assert_eq!(budget.llm_calls, 0);
        assert!(budget.has_budget());
    }

    #[test]
    fn token_budget_unlimited_never_fails() {
        let mut budget = TokenBudget::default();
        // No max set — should always succeed.
        for _ in 0..100 {
            assert!(budget.record(1_000_000).is_ok());
        }
        assert!(budget.has_budget());
    }

    // -----------------------------------------------------------------------
    // Retry logic tests
    // -----------------------------------------------------------------------

    #[test]
    fn retryable_error_detection() {
        assert!(Agent::is_retryable(&DysonError::Llm("rate limited".into())));
        assert!(Agent::is_retryable(&DysonError::Llm("HTTP 429".into())));
        assert!(Agent::is_retryable(&DysonError::Llm("overloaded".into())));
        assert!(Agent::is_retryable(&DysonError::Llm("HTTP 529".into())));
        assert!(Agent::is_retryable(&DysonError::Llm("HTTP 503".into())));

        // Non-retryable errors.
        assert!(!Agent::is_retryable(&DysonError::Llm("authentication failed".into())));
        assert!(!Agent::is_retryable(&DysonError::Config("bad config".into())));
        assert!(!Agent::is_retryable(&DysonError::Cancelled));
    }

    #[tokio::test]
    async fn token_budget_stops_agent_loop() {
        // LLM reports 100 tokens per turn. Budget is 150, so it should
        // stop after the second turn (100 + 100 = 200 > 150).
        let llm = MockLlm::new(vec![
            // Turn 1: tool call (100 tokens).
            vec![
                StreamEvent::ToolUseStart {
                    id: "call_1".into(),
                    name: "bash".into(),
                },
                StreamEvent::ToolUseComplete {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo hi"}),
                },
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::ToolUse,
                    output_tokens: Some(100),
                },
            ],
            // Turn 2: tool call (100 more tokens → over budget).
            vec![
                StreamEvent::ToolUseStart {
                    id: "call_2".into(),
                    name: "bash".into(),
                },
                StreamEvent::ToolUseComplete {
                    id: "call_2".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo bye"}),
                },
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::ToolUse,
                    output_tokens: Some(100),
                },
            ],
        ]);

        let settings = AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        };

        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None))];
        let sandbox: Box<dyn Sandbox> = Box::new(DangerousNoSandbox);
        let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();
        agent.token_budget.max_output_tokens = Some(150);
        let mut output = MockOutput::new();

        // Agent should stop due to budget, not error out.
        let _result = agent.run("run both", &mut output).await.unwrap();
        assert!(agent.token_budget.output_tokens_used >= 200);
        assert!(!agent.token_budget.has_budget());
    }

    // -------------------------------------------------------------------
    // Input token tracking
    // -------------------------------------------------------------------

    #[test]
    fn token_budget_tracks_input_tokens() {
        let mut budget = TokenBudget::default();
        assert_eq!(budget.input_tokens_used, 0);

        budget.record_input(500);
        assert_eq!(budget.input_tokens_used, 500);

        budget.record_input(300);
        assert_eq!(budget.input_tokens_used, 800);
    }

    #[test]
    fn token_budget_reset_clears_input_tokens() {
        let mut budget = TokenBudget::default();
        budget.record_input(1000);
        budget.record(200).unwrap();
        budget.reset();
        assert_eq!(budget.input_tokens_used, 0);
        assert_eq!(budget.output_tokens_used, 0);
        assert_eq!(budget.llm_calls, 0);
    }

    // -------------------------------------------------------------------
    // ToolMode enum
    // -------------------------------------------------------------------

    #[test]
    fn tool_mode_execute_vs_observe() {
        assert_ne!(crate::llm::ToolMode::Execute, crate::llm::ToolMode::Observe);
        // Copy semantics work.
        let mode = crate::llm::ToolMode::Observe;
        let copied = mode;
        assert_eq!(mode, copied);
    }
}
