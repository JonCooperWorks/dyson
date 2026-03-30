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
// SilentOutput — discards all output (used by self-improvement reflection).
// ---------------------------------------------------------------------------

/// A no-op output sink used for side-channel LLM calls where we want
/// tool execution but don't need to stream text to the user.
struct SilentOutput;

impl crate::controller::Output for SilentOutput {
    fn text_delta(&mut self, _: &str) -> Result<()> { Ok(()) }
    fn tool_use_start(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
    fn tool_use_complete(&mut self) -> Result<()> { Ok(()) }
    fn tool_result(&mut self, _: &ToolOutput) -> Result<()> { Ok(()) }
    fn send_file(&mut self, _: &std::path::Path) -> Result<()> { Ok(()) }
    fn error(&mut self, _: &DysonError) -> Result<()> { Ok(()) }
    fn flush(&mut self) -> Result<()> { Ok(()) }
}

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
    ///
    /// Wrapped in `Arc` (not `Box`) so subagents can share the parent's
    /// sandbox without cloning.  This ensures child agents inherit the
    /// same security policy — subagents cannot bypass the sandbox.
    sandbox: Arc<dyn Sandbox>,

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

    /// Estimated token threshold for automatic context compaction.
    /// Before each LLM call, the agent estimates the current context size
    /// offline; if it exceeds this value, the conversation is compacted first.
    compaction_threshold: Option<usize>,
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
        sandbox: Arc<dyn Sandbox>,
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
            depth: 0,
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
            compaction_threshold: settings.compaction_threshold,
        })
    }

    /// Get a shared reference to the sandbox for subagent reuse.
    ///
    /// Subagents share the parent's sandbox to ensure consistent security
    /// policy across the agent hierarchy.
    pub fn sandbox(&self) -> &Arc<dyn Sandbox> {
        &self.sandbox
    }

    /// Set the subagent nesting depth on this agent's tool context.
    ///
    /// Called by `SubagentTool` after construction to propagate the depth
    /// counter.  The child runs at `parent_depth + 1`.
    pub fn set_depth(&mut self, depth: u8) {
        self.tool_context.depth = depth;
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

    /// Compact the conversation by summarising it and replacing the history.
    ///
    /// This is the core context-compaction primitive:
    ///
    /// 1. Send the full message history to the LLM with a summarisation
    ///    prompt (no tools, so the model can only respond with text).
    /// 2. Replace the entire message history with a single user message
    ///    containing the summary, prefixed with `[Context Summary]`.
    /// 3. The next LLM call sees only the compact summary instead of the
    ///    full history, dramatically reducing input tokens.
    ///
    /// ## When to use
    ///
    /// - Automatically: the agent loop triggers compaction when the
    ///   offline-estimated context size exceeds `compaction_threshold`.
    /// - Manually: a controller can call `agent.compact()` directly
    ///   (e.g. in response to a `/compact` command).
    ///
    /// ## Design notes
    ///
    /// The summary is injected as a `User` message so it plays well with
    /// every provider's message format (the first message must be a user
    /// message for Anthropic).  The `[Context Summary]` prefix tells the
    /// model that this is condensed history, not a literal user utterance.
    pub async fn compact(&mut self, output: &mut dyn Output) -> Result<()> {
        if self.messages.is_empty() {
            return Ok(());
        }

        tracing::info!(
            messages = self.messages.len(),
            estimated_tokens = self.estimate_context_tokens(&self.system_prompt),
            "compacting conversation context"
        );

        // Build a one-shot summarisation request: the full history with a
        // system prompt that instructs the model to produce a concise summary.
        let compaction_system = format!(
            "{}\n\n\
             You are being asked to summarise the conversation so far.  \
             Produce a concise but thorough summary that preserves:\n\
             - Key facts, decisions, and conclusions reached\n\
             - Important tool results and their outcomes\n\
             - The user's original goals and current progress\n\
             - Any pending tasks or unresolved questions\n\n\
             Write the summary as a single block of text.  Do NOT call any tools.  \
             Do NOT ask questions.  Just summarise.",
            self.system_prompt,
        );

        let empty_tools: &[ToolDefinition] = &[];
        let response = self
            .client
            .stream(&self.messages, &compaction_system, empty_tools, &self.config)
            .await?;

        let (assistant_msg, _tool_calls, _output_tokens) =
            stream_handler::process_stream(response.stream, output).await?;

        // Extract the summary text from the assistant's response.
        let summary = assistant_msg
            .content
            .iter()
            .filter_map(|block| {
                if let crate::message::ContentBlock::Text { text } = block {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        if summary.is_empty() {
            tracing::warn!("compaction produced empty summary — keeping original history");
            return Ok(());
        }

        let old_count = self.messages.len();

        // Rotate: replace the entire history with the summary.
        self.messages.clear();
        self.messages.push(Message::user(&format!(
            "[Context Summary]\n\n{summary}"
        )));

        // Reset token counters since we've effectively started a new context.
        self.token_budget.reset();

        tracing::info!(
            old_messages = old_count,
            summary_chars = summary.len(),
            "context compacted successfully"
        );

        Ok(())
    }

    /// Estimate the total token count of the current context that would be
    /// sent to the LLM (messages + system prompt + tool definitions).
    ///
    /// This is a local/offline estimate using whitespace splitting — no API
    /// call needed.  Used to decide whether to compact before the next call.
    fn estimate_context_tokens(&self, system_prompt: &str) -> usize {
        let system_tokens = system_prompt.split_whitespace().count();

        let message_tokens: usize = self.messages.iter().map(|m| m.estimate_tokens()).sum();

        let tool_tokens: usize = self
            .tool_definitions
            .iter()
            .map(|t| {
                t.name.split_whitespace().count()
                    + t.description.split_whitespace().count()
                    + t.input_schema.to_string().split_whitespace().count()
                    + 10 // per-tool JSON framing overhead
            })
            .sum();

        system_tokens + message_tokens + tool_tokens
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
        let mut hit_max_iterations = false;

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
            // -- Auto-compact if estimated context tokens exceed threshold --
            //
            // Before each LLM call, estimate the token count of the full
            // context (messages + system prompt + tool definitions) locally.
            // If it exceeds the threshold, compact first so we never send
            // an oversized context to the API.
            if let Some(threshold) = self.compaction_threshold
                && self.messages.len() >= 3
            {
                let estimated_tokens = self.estimate_context_tokens(&turn_system_prompt);
                if estimated_tokens > threshold {
                    tracing::info!(
                        estimated_tokens = estimated_tokens,
                        threshold = threshold,
                        messages = self.messages.len(),
                        "estimated context tokens exceed compaction threshold — compacting"
                    );
                    if let Err(e) = self.compact(output).await {
                        tracing::warn!(
                            error = %e,
                            "auto-compaction failed — continuing with full history"
                        );
                    }
                }
            }

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
                    "agent hit maximum iterations — requesting summary"
                );
                hit_max_iterations = true;
            }
        }

        // When the agent exhausts max_iterations, make one final LLM call
        // (with no tools) so the model can summarise progress gracefully.
        if hit_max_iterations {
            self.messages.push(Message::user(
                "You have reached the maximum number of iterations and must stop now. \
                 Please provide a brief summary of:\n\
                 1. What you have accomplished so far\n\
                 2. What still needs to be done\n\
                 3. Any relevant partial results\n\n\
                 Do NOT call any tools. Just summarize.",
            ));

            let empty_tools: &[crate::llm::ToolDefinition] = &[];
            match self
                .client
                .stream(
                    &self.messages,
                    &turn_system_prompt,
                    empty_tools,
                    &self.config,
                )
                .await
            {
                Ok(response) => {
                    let (assistant_msg, _tool_calls, _output_tokens) =
                        stream_handler::process_stream(response.stream, output).await?;
                    for block in &assistant_msg.content {
                        if let crate::message::ContentBlock::Text { text } = block {
                            final_text = text.clone();
                        }
                    }
                    self.messages.push(assistant_msg);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "summary LLM call failed — falling back to error");
                    output.error(&DysonError::Llm(format!(
                        "Reached maximum iterations ({}) — stopping",
                        self.max_iterations
                    )))?;
                }
            }
        }

        // -- Self-improvement reflection --
        //
        // Every 2N turns, fire a side-channel LLM call that reviews the
        // conversation and decides whether to create/improve skills or
        // export training data.  This is a separate call with its own
        // system prompt — it doesn't pollute the main conversation history.
        if self.nudge_interval > 0
            && self.turn_count > self.nudge_interval
            && self.turn_count.is_multiple_of(self.nudge_interval * 2)
            && self.tool_context.workspace.is_some()
        {
            if let Err(e) = self.self_improve(output).await {
                tracing::warn!(
                    error = %e,
                    "self-improvement reflection failed — continuing normally"
                );
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

    /// Run a side-channel self-improvement reflection.
    ///
    /// Makes a separate LLM call with a focused system prompt and only
    /// the `skill_create` and `export_conversation` tools.  The LLM
    /// reviews the conversation and decides whether to:
    ///
    /// - Create a new skill from a complex task it just solved
    /// - Improve an existing skill based on what it learned
    /// - Export the conversation as training data
    /// - Do nothing (if the conversation was trivial)
    ///
    /// This runs in a separate message context — nothing from this call
    /// is added to the main conversation history.  The user sees tool
    /// use output (skill created, export written) but the reflection
    /// reasoning is invisible.
    ///
    /// ## Design
    ///
    /// Unlike the memory nudge (which injects a user message and hopes
    /// the model acts on it), this is a real LLM call with real tool
    /// execution.  The model either acts or doesn't — there's no
    /// relying on the model noticing a hint in context.
    async fn self_improve(&mut self, output: &mut dyn Output) -> Result<()> {
        tracing::info!(
            turn_count = self.turn_count,
            messages = self.messages.len(),
            "running self-improvement reflection"
        );

        // Log to the user that reflection is happening.
        let _ = output.text_delta("\n\n[Self-improvement: reflecting on conversation...]\n");
        let _ = output.flush();

        // Build a condensed view of the conversation for the reflection.
        let reflection_system = Self::build_reflection_system_prompt(&self.tool_context).await;

        // Only expose self-improvement tools.
        let reflection_tools: Vec<ToolDefinition> = self
            .tool_definitions
            .iter()
            .filter(|t| t.name == "skill_create" || t.name == "export_conversation")
            .cloned()
            .collect();

        if reflection_tools.is_empty() {
            tracing::warn!("self-improvement tools not loaded — skipping reflection");
            return Ok(());
        }

        // Build the reflection messages: a condensed summary of what happened.
        let summary = Self::summarize_for_reflection(&self.messages);
        tracing::debug!(
            summary_len = summary.len(),
            "built reflection summary"
        );

        let reflection_messages = vec![
            Message::user(&summary),
        ];

        // LLM call with tool loop — if it wants to use tools, run them.
        // Cap at 3 iterations to prevent runaway loops.
        let mut messages = reflection_messages;
        let mut actions_taken = 0usize;

        for iteration in 0..3u8 {
            tracing::info!(
                iteration = iteration,
                "self-improvement LLM call"
            );

            let response = match self
                .client
                .stream(&messages, &reflection_system, &reflection_tools, &self.config)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "self-improvement LLM call failed");
                    let _ = output.text_delta("[Self-improvement: LLM call failed, skipping]\n");
                    return Ok(()); // non-fatal
                }
            };

            // Process stream silently — we only care about tool calls.
            let mut silent_output = SilentOutput;
            let (assistant_msg, tool_calls, _tokens) =
                stream_handler::process_stream(response.stream, &mut silent_output).await?;

            // Log what the model said (for debugging), even though we don't
            // show it to the user.
            for block in &assistant_msg.content {
                if let crate::message::ContentBlock::Text { text } = block {
                    if !text.trim().is_empty() {
                        tracing::info!(
                            reasoning = text.as_str(),
                            "self-improvement reasoning"
                        );
                    }
                }
            }

            messages.push(assistant_msg);

            if tool_calls.is_empty() {
                tracing::info!("self-improvement: model decided no action needed");
                break;
            }

            // Execute the tool calls (through the sandbox like everything else).
            for call in &tool_calls {
                tracing::info!(
                    tool = call.name.as_str(),
                    "self-improvement executing tool"
                );

                let result = self.execute_tool_call_timed(call).await;
                let tool_result_msg = match result {
                    Ok(ref tool_output) => {
                        actions_taken += 1;

                        // Show tool activity to the user so they see what was
                        // auto-created (skill files, exports).
                        let _ = output.tool_use_start(&call.name, &call.id);
                        let _ = output.tool_result(tool_output);
                        let _ = output.tool_use_complete();

                        tracing::info!(
                            tool = call.name.as_str(),
                            is_error = tool_output.is_error,
                            result = tool_output.content.as_str(),
                            "self-improvement tool result"
                        );

                        Message::tool_result(&call.id, &tool_output.content, tool_output.is_error)
                    }
                    Err(ref e) => {
                        tracing::warn!(
                            tool = call.name.as_str(),
                            error = %e,
                            "self-improvement tool call failed"
                        );
                        let _ = output.text_delta(
                            &format!("[Self-improvement: {} failed: {}]\n", call.name, e)
                        );
                        Message::tool_result(&call.id, &e.to_string(), true)
                    }
                };
                messages.push(tool_result_msg);
            }
        }

        if actions_taken == 0 {
            let _ = output.text_delta("[Self-improvement: no action needed]\n");
        } else {
            let _ = output.text_delta(
                &format!("[Self-improvement: {actions_taken} action(s) taken]\n")
            );
        }
        let _ = output.flush();

        // Persist the full reflection exchange to the workspace's
        // improvement/ directory so the user can inspect it later.
        self.save_reflection_log(
            &reflection_system,
            &messages,
            actions_taken,
        ).await;

        tracing::info!(
            actions_taken = actions_taken,
            "self-improvement reflection complete"
        );
        Ok(())
    }

    /// Save a reflection exchange to the workspace for later inspection.
    ///
    /// Writes the full system prompt, messages, and metadata as JSON to
    /// `improvement/<timestamp>.json` in the workspace.  This lets users
    /// review what the self-improvement engine decided and why.
    async fn save_reflection_log(
        &self,
        system_prompt: &str,
        messages: &[Message],
        actions_taken: usize,
    ) {
        let Some(ref ws) = self.tool_context.workspace else {
            return;
        };

        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let log = serde_json::json!({
            "timestamp": epoch,
            "turn_count": self.turn_count,
            "actions_taken": actions_taken,
            "system_prompt": system_prompt,
            "messages": messages,
        });

        let content = serde_json::to_string_pretty(&log).unwrap_or_default();
        let file_key = format!("improvement/{epoch}.json");

        let mut ws = ws.write().await;
        ws.set(&file_key, &content);
        if let Err(e) = ws.save() {
            tracing::warn!(
                error = %e,
                file = file_key.as_str(),
                "failed to save reflection log"
            );
        } else {
            tracing::info!(
                file = file_key.as_str(),
                actions_taken = actions_taken,
                "saved reflection log"
            );
        }
    }

    /// Build the system prompt for the self-improvement reflection call.
    async fn build_reflection_system_prompt(ctx: &ToolContext) -> String {
        // List existing skills so the model knows what already exists.
        let existing_skills = if let Some(ref ws) = ctx.workspace {
            let ws = ws.read().await;
            let skill_files = ws.skill_files();
            if skill_files.is_empty() {
                "No skills exist yet.".to_string()
            } else {
                let names: Vec<String> = skill_files
                    .iter()
                    .filter_map(|p| {
                        p.file_stem()
                            .and_then(|s| s.to_str())
                            .map(String::from)
                    })
                    .collect();
                format!("Existing skills: {}", names.join(", "))
            }
        } else {
            "No workspace configured.".to_string()
        };

        format!(
            "You are a self-improvement engine for an AI agent.  Your job is to review \
             a conversation that just happened and decide whether to take action.\n\n\
             You have two tools:\n\
             - **skill_create**: Create or improve a SKILL.md file in the workspace.  \
               Skills are system prompt fragments that auto-load on startup, teaching \
               the agent reusable procedures.\n\
             - **export_conversation**: Export the conversation as ShareGPT-format \
               training data for fine-tuning tool-calling models.\n\n\
             ## When to create a skill\n\
             - The agent solved a complex, multi-step task that it might encounter again\n\
             - The agent discovered a non-obvious procedure or debugging pattern\n\
             - The agent found a domain-specific workflow worth encoding\n\n\
             ## When to improve a skill\n\
             - An existing skill's instructions were insufficient and the agent had to \
               improvise — capture what worked\n\
             - The agent found a better approach than what the skill describes\n\n\
             ## When to export\n\
             - The conversation contains substantial, high-quality tool use (3+ tool \
               calls with successful outcomes)\n\
             - The conversation demonstrates a complete problem-solving trajectory\n\n\
             ## When to do nothing\n\
             - The conversation was trivial (simple Q&A, one-step tasks)\n\
             - A matching skill already exists and doesn't need improvement\n\
             - The conversation was mostly errors or failed attempts\n\n\
             Doing nothing is the right choice most of the time.  Only act when there's \
             genuine value in persisting knowledge or exporting data.\n\n\
             {existing_skills}"
        )
    }

    /// Summarize the conversation for the reflection LLM call.
    ///
    /// Instead of sending the full message history (which could be huge),
    /// build a condensed representation: user goals, tools used, outcomes.
    fn summarize_for_reflection(messages: &[Message]) -> String {
        let mut summary = String::from(
            "Review this conversation and decide whether to create/improve a skill \
             or export training data.  Here is what happened:\n\n"
        );

        let mut tool_call_count = 0;
        let mut tool_error_count = 0;
        let mut tools_used: Vec<String> = Vec::new();

        for msg in messages {
            for block in &msg.content {
                match block {
                    crate::message::ContentBlock::Text { text } => {
                        // Include user messages and short assistant text.
                        let role = match msg.role {
                            crate::message::Role::User => "User",
                            crate::message::Role::Assistant => "Assistant",
                        };
                        // Truncate long text to keep the summary compact.
                        let truncated = if text.len() > 500 {
                            format!("{}...[truncated]", &text[..500])
                        } else {
                            text.clone()
                        };
                        summary.push_str(&format!("{role}: {truncated}\n\n"));
                    }
                    crate::message::ContentBlock::ToolUse { name, .. } => {
                        tool_call_count += 1;
                        if !tools_used.contains(name) {
                            tools_used.push(name.clone());
                        }
                        summary.push_str(&format!("[Tool call: {name}]\n"));
                    }
                    crate::message::ContentBlock::ToolResult { is_error, content, .. } => {
                        if *is_error {
                            tool_error_count += 1;
                            summary.push_str(&format!("[Tool error: {}]\n", &content[..content.len().min(200)]));
                        } else {
                            let truncated = if content.len() > 200 {
                                format!("{}...", &content[..200])
                            } else {
                                content.clone()
                            };
                            summary.push_str(&format!("[Tool result: {truncated}]\n"));
                        }
                    }
                }
            }
        }

        summary.push_str(&format!(
            "\n---\nStats: {tool_call_count} tool calls ({tool_error_count} errors), \
             tools used: [{}], {} messages total.",
            tools_used.join(", "),
            messages.len(),
        ));

        summary
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
        sent_files: Vec<std::path::PathBuf>,
    }

    impl MockOutput {
        fn new() -> Self {
            Self {
                text: String::new(),
                sent_files: Vec::new(),
            }
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
        fn send_file(&mut self, path: &std::path::Path) -> Result<()> {
            self.sent_files.push(path.to_path_buf());
            Ok(())
        }
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
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
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
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
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
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
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

    #[tokio::test]
    async fn reflection_system_prompt_lists_tools() {
        let ctx = crate::tool::ToolContext {
            working_dir: std::env::temp_dir(),
            env: HashMap::new(),
            cancellation: CancellationToken::new(),
            workspace: None,
            depth: 0,
        };
        let prompt = Agent::build_reflection_system_prompt(&ctx).await;
        assert!(prompt.contains("skill_create"));
        assert!(prompt.contains("export_conversation"));
        assert!(prompt.contains("When to create a skill"));
        assert!(prompt.contains("When to do nothing"));
    }

    #[test]
    fn summarize_for_reflection_captures_tool_stats() {
        let messages = vec![
            Message::user("Deploy my app"),
            Message::assistant(vec![
                crate::message::ContentBlock::Text {
                    text: "I'll deploy it.".into(),
                },
                crate::message::ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "deploy.sh"}),
                },
            ]),
            Message::tool_result("c1", "Deployed successfully", false),
            Message::assistant(vec![crate::message::ContentBlock::Text {
                text: "Done!".into(),
            }]),
        ];

        let summary = Agent::summarize_for_reflection(&messages);
        assert!(summary.contains("Deploy my app"));
        assert!(summary.contains("[Tool call: bash]"));
        assert!(summary.contains("Deployed successfully"));
        assert!(summary.contains("1 tool calls (0 errors)"));
        assert!(summary.contains("bash"));
        assert!(summary.contains("4 messages total"));
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
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
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

    // -----------------------------------------------------------------------
    // File sending tests
    // -----------------------------------------------------------------------

    /// A mock tool that returns a ToolOutput with attached files.
    struct MockFileTool;

    #[async_trait::async_trait]
    impl crate::tool::Tool for MockFileTool {
        fn name(&self) -> &str { "send_test_file" }
        fn description(&self) -> &str { "Returns a file" }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {},
            })
        }
        async fn run(
            &self,
            _input: serde_json::Value,
            _ctx: &crate::tool::ToolContext,
        ) -> Result<ToolOutput> {
            Ok(ToolOutput::success("Here is your file.")
                .with_file("/tmp/test_report.pdf")
                .with_file("/tmp/data.csv"))
        }
    }

    /// A skill that provides only the MockFileTool.
    struct MockFileSkill {
        tools: Vec<Arc<dyn crate::tool::Tool>>,
    }

    impl MockFileSkill {
        fn new() -> Self {
            Self {
                tools: vec![Arc::new(MockFileTool)],
            }
        }
    }

    #[async_trait::async_trait]
    impl Skill for MockFileSkill {
        fn name(&self) -> &str { "mock_file_skill" }
        fn tools(&self) -> &[Arc<dyn crate::tool::Tool>] {
            &self.tools
        }
    }

    #[tokio::test]
    async fn tool_output_files_dispatched_via_send_file() {
        // LLM calls send_test_file, then responds with text.
        let llm = MockLlm::new(vec![
            vec![
                StreamEvent::ToolUseStart {
                    id: "call_f1".into(),
                    name: "send_test_file".into(),
                },
                StreamEvent::ToolUseComplete {
                    id: "call_f1".into(),
                    name: "send_test_file".into(),
                    input: serde_json::json!({}),
                },
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::ToolUse,
                    output_tokens: None,
                },
            ],
            vec![
                StreamEvent::TextDelta("Files sent.".into()),
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

        let skills: Vec<Box<dyn Skill>> = vec![Box::new(MockFileSkill::new())];
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
        let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();
        let mut output = MockOutput::new();

        let result = agent.run("send me a file", &mut output).await.unwrap();
        assert_eq!(result, "Files sent.");

        // Verify that send_file was called for both attached files.
        assert_eq!(output.sent_files.len(), 2);
        assert_eq!(output.sent_files[0], std::path::PathBuf::from("/tmp/test_report.pdf"));
        assert_eq!(output.sent_files[1], std::path::PathBuf::from("/tmp/data.csv"));
    }

    #[tokio::test]
    async fn tool_output_no_files_means_no_send_file() {
        // Normal tool call without files — send_file should not be called.
        let llm = MockLlm::new(vec![
            vec![
                StreamEvent::ToolUseStart {
                    id: "call_1".into(),
                    name: "bash".into(),
                },
                StreamEvent::ToolUseComplete {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo hello"}),
                },
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::ToolUse,
                    output_tokens: None,
                },
            ],
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
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
        let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();
        let mut output = MockOutput::new();

        agent.run("echo hello", &mut output).await.unwrap();

        // No files should have been sent.
        assert!(output.sent_files.is_empty());
    }

    #[test]
    fn tool_mode_execute_vs_observe() {
        assert_ne!(crate::llm::ToolMode::Execute, crate::llm::ToolMode::Observe);
        // Copy semantics work.
        let mode = crate::llm::ToolMode::Observe;
        let copied = mode;
        assert_eq!(mode, copied);
    }

    // -----------------------------------------------------------------------
    // Context compaction tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn compact_replaces_history_with_summary() {
        // Set up an agent with some conversation history, then compact it.
        // The MockLlm needs:
        //   1. Response for the initial run() call
        //   2. Response for the compact() summarisation call
        let llm = MockLlm::new(vec![
            // Turn 1: normal response.
            vec![
                StreamEvent::TextDelta("Hello! I can help you.".into()),
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    output_tokens: None,
                },
            ],
            // Compaction: summarisation response.
            vec![
                StreamEvent::TextDelta("The user greeted the assistant and received a helpful response.".into()),
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
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
        let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();
        let mut output = MockOutput::new();

        // Run a turn to build up history.
        agent.run("hi there", &mut output).await.unwrap();
        assert_eq!(agent.messages.len(), 2); // user + assistant

        // Compact.
        agent.compact(&mut output).await.unwrap();

        // After compaction: exactly 1 message (the summary).
        assert_eq!(agent.messages.len(), 1);
        assert_eq!(agent.messages[0].role, crate::message::Role::User);
        match &agent.messages[0].content[0] {
            crate::message::ContentBlock::Text { text } => {
                assert!(text.starts_with("[Context Summary]"));
                assert!(text.contains("greeted the assistant"));
            }
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn compact_on_empty_history_is_noop() {
        // Compacting with no messages should succeed without making an LLM call.
        let llm = MockLlm::new(vec![]); // No responses queued — would panic if called.

        let settings = AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        };

        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None))];
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
        let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();
        let mut output = MockOutput::new();

        agent.compact(&mut output).await.unwrap();
        assert!(agent.messages.is_empty());
    }

    #[tokio::test]
    async fn compact_resets_token_budget() {
        let llm = MockLlm::new(vec![
            // Turn 1.
            vec![
                StreamEvent::TextDelta("OK.".into()),
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    output_tokens: Some(50),
                },
            ],
            // Compaction summary.
            vec![
                StreamEvent::TextDelta("Summary of the conversation.".into()),
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    output_tokens: Some(10),
                },
            ],
        ]);

        let settings = AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        };

        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None))];
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
        let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();
        let mut output = MockOutput::new();

        agent.run("hello", &mut output).await.unwrap();
        assert_eq!(agent.token_budget.output_tokens_used, 50);
        assert_eq!(agent.token_budget.llm_calls, 1);

        agent.compact(&mut output).await.unwrap();

        // Token budget should be reset after compaction.
        assert_eq!(agent.token_budget.output_tokens_used, 0);
        assert_eq!(agent.token_budget.input_tokens_used, 0);
        assert_eq!(agent.token_budget.llm_calls, 0);
    }

    #[tokio::test]
    async fn auto_compaction_triggers_on_threshold() {
        // Set up a very low compaction threshold so that after turn 1 builds
        // up history, the offline token estimate exceeds it on turn 2.
        // The MockLlm needs:
        //   1. First run() response
        //   2. Compaction summary (triggered automatically on second run)
        //   3. Second run() response
        let llm = MockLlm::new(vec![
            // Turn 1: normal response.
            vec![
                StreamEvent::TextDelta("First response.".into()),
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    output_tokens: Some(20),
                },
            ],
            // Auto-compaction summary (triggered at start of turn 2 loop
            // because estimated context tokens exceed the low threshold).
            vec![
                StreamEvent::TextDelta("Summary of turn 1.".into()),
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    output_tokens: Some(5),
                },
            ],
            // Turn 2: normal response after compaction.
            vec![
                StreamEvent::TextDelta("Second response.".into()),
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    output_tokens: Some(20),
                },
            ],
        ]);

        let settings = AgentSettings {
            api_key: "test".into(),
            compaction_threshold: Some(10), // very low — system prompt + tool defs alone exceed this
            ..Default::default()
        };

        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None))];
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
        let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();
        let mut output = MockOutput::new();

        // First turn — build up history (2 messages: user + assistant).
        agent.run("first message", &mut output).await.unwrap();
        assert_eq!(agent.messages.len(), 2);

        // Second turn — pushes "second message" (3 msgs total), offline
        // estimate exceeds threshold of 10, so auto-compact fires.
        let result = agent.run("second message", &mut output).await.unwrap();
        assert_eq!(result, "Second response.");

        // Trace: run("second message") → push user (3 msgs) → loop →
        // estimate > 10, compact → messages=[summary] → LLM → push assistant
        // Result: [summary, assistant] = 2 messages.
        assert_eq!(agent.messages.len(), 2); // summary + assistant
    }
}
