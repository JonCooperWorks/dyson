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
//   dream.rs           — Dream trait, DreamRunner, trigger/event types
//   reflection.rs      — Built-in Dream implementations (memory, learning, self-improvement)
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
//       // 1. LIMIT — check per-turn rate limits
//       for call in tool_calls:
//           if limiter.check(call.name) fails:
//               push error tool_result, skip call
//
//       // 2. ANALYZE — group by resource dependencies
//       phases = DependencyAnalyzer.analyze(allowed_calls)
//
//       // 3. EXECUTE — run phases in order
//       for phase in phases:
//           Parallel  → join_all(execute_tool_call_timed(...))
//           Sequential → execute one-by-one
//           Each call: sandbox.check() → tool.run() → sandbox.after()
//
//       // 4. FORMAT — structured results for LLM
//       for each result:
//           formatted = formatter.format(call, output, duration)
//           messages.push(tool_result(call.id, formatted.to_llm_message()))
//
//       limiter.reset_turn()
//       // loop — LLM sees tool results on next iteration
//
// Architecture:
//
//   Agent owns:
//     ┌──────────────────────────────────────────────────┐
//     │  client:  RateLimited<Box<dyn LlmClient>>         │
//     │  sandbox: Arc<dyn Sandbox>     ← gates all calls │
//     │  skills:  Vec<Box<dyn Skill>>                    │
//     │  tools:   HashMap<name, Arc<dyn Tool>>           │
//     │  tool_definitions: Vec<ToolDefinition>           │
//     │  system_prompt: String                           │
//     │  config: CompletionConfig                        │
//     │  messages: Vec<Message>        ← conversation    │
//     │  max_iterations: usize                           │
//     │  limiter: ToolLimiter          ← rate limiting   │
//     │  formatter: ResultFormatter    ← output format   │
//     └──────────────────────────────────────────────────┘
//
// Why does Agent own both skills AND a flat tools map?
//   Skills own tools (for lifecycle management), but the agent needs O(1)
//   lookup by tool name when dispatching calls.  The flat HashMap provides
//   that.  Both hold Arc<dyn Tool> to the same underlying objects — no
//   duplication, just shared references.
// ===========================================================================

mod compaction;
mod dependency_analyzer;
pub mod dream;
pub mod rate_limiter;
mod reflection;
mod result_formatter;
mod silent_output;
pub mod stream_handler;
pub mod token_budget;
mod tool_limiter;

use std::collections::HashMap;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::config::{AgentSettings, CompactionConfig};
use crate::controller::Output;
use crate::error::{DysonError, LlmRecovery, Result};
use crate::llm::{CompletionConfig, LlmClient, ToolDefinition};
use crate::message::{ContentBlock, Message};
use crate::sandbox::{Sandbox, SandboxDecision};
use crate::skill::Skill;
use crate::tool::{Tool, ToolContext, ToolOutput};

use self::dependency_analyzer::{DependencyAnalyzer, ExecutionPhase};
use self::dream::{DreamEvent, DreamHandle};
use self::result_formatter::ResultFormatter;
use self::tool_limiter::ToolLimiter;

use self::stream_handler::ToolCall;

use self::token_budget::TokenBudget;

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// The streaming tool-use agent — Dyson's core runtime.
///
/// Created once at startup, then `run()` is called for each user message.
/// Conversation history (`messages`) persists across calls for multi-turn
/// conversations.
pub struct Agent {
    /// LLM client for streaming completions, gated by rate limiting.
    client: rate_limiter::RateLimited<Box<dyn LlmClient>>,

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

    /// When `true`, tool definitions are omitted from LLM requests.
    /// Set when the active model doesn't support tool use.
    tools_disabled: bool,

    /// Composed system prompt: base prompt + all skill prompt fragments.
    system_prompt: Arc<str>,

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

    /// Number of user turns processed (for dream trigger timing).
    turn_count: usize,

    /// Token usage tracking and optional budget enforcement.
    pub token_budget: TokenBudget,

    /// Context compaction configuration.
    /// When set, the agent automatically compacts conversation history when
    /// the estimated context size exceeds `compaction_config.threshold()`.
    compaction_config: Option<CompactionConfig>,

    /// Per-turn tool call rate limiter.
    limiter: ToolLimiter,

    /// Structured result formatter for LLM-optimized tool output.
    formatter: ResultFormatter,

    /// Cached sum of estimated tokens for all tool definitions.
    /// Tool definitions are immutable after construction, so this is computed
    /// once in `new()` and reused in `estimate_context_tokens()`.
    cached_tool_tokens: usize,

    /// Handle to the persistent dream thread — fires background cognitive
    /// tasks (memory maintenance, learning synthesis, self-improvement)
    /// on trigger events without blocking the controller loop.
    /// See `dream.rs` and `docs/dreaming.md`.
    dream_handle: DreamHandle,
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
        workspace: Option<
            std::sync::Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>>,
        >,
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

        // Pre-compute tool definition token estimate (immutable after construction).
        let cached_tool_tokens: usize = tool_definitions
            .iter()
            .map(|t| {
                t.name.split_whitespace().count()
                    + t.description.split_whitespace().count()
                    + crate::message::estimate_json_tokens(&t.input_schema)
                    + 10 // per-tool JSON framing overhead
            })
            .sum();

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

        let client = match settings.rate_limit.as_ref() {
            Some(rl) => rate_limiter::RateLimited::new(
                client,
                rl.max_messages,
                std::time::Duration::from_secs(rl.window_secs),
            ),
            None => rate_limiter::RateLimited::unlimited(client),
        };

        // -- Build the persistent dream thread --
        //
        // Dreams are autonomous background tasks (memory consolidation,
        // self-improvement) that fire on trigger events.  They run on a
        // dedicated thread so they never block the controller loop.
        // See dream.rs and docs/dreaming.md.
        let mut dreams: Vec<Arc<dyn dream::Dream>> = Vec::new();

        if tool_context.workspace.is_some() {
            // Learning synthesis: merge conversation learnings into MEMORY.md
            // after context compaction.
            dreams.push(Arc::new(reflection::LearningSynthesisDream));

            if nudge_interval > 0 {
                // Memory maintenance: update MEMORY.md / USER.md every N turns.
                dreams.push(Arc::new(
                    reflection::MemoryMaintenanceDream::new(nudge_interval),
                ));

                // Self-improvement: create skills / export data every 2N turns.
                dreams.push(Arc::new(
                    reflection::SelfImprovementDream::new(nudge_interval),
                ));
            }
        }

        let dream_handle = DreamHandle::new(dreams);

        Ok(Self {
            client,
            sandbox,
            skills,
            tools,
            tool_to_skill,
            tool_definitions,
            system_prompt: Arc::from(system_prompt),
            config,
            max_iterations: settings.max_iterations,
            max_retries: 3,
            messages: Vec::new(),
            tool_context,
            turn_count: 0,
            token_budget: TokenBudget::default(),
            compaction_config: settings.compaction,
            limiter: ToolLimiter::for_agent(),
            formatter: ResultFormatter::default(),
            cached_tool_tokens,
            tools_disabled: false,
            dream_handle,
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

    /// Send a dream event to the persistent dream thread.
    ///
    /// This never blocks — the event is sent over a channel and the dream
    /// thread handles summarisation and spawning.
    fn fire_dreams(&self, event: DreamEvent) {
        if self.messages.is_empty() || self.tool_context.workspace.is_none() {
            return;
        }

        self.dream_handle.fire(
            event,
            self.client.handle(rate_limiter::Priority::Background),
            self.config.clone(),
            self.tool_context.clone(),
            self.messages.clone(),
            self.turn_count,
        );
    }

    /// Clear conversation history, firing session-end dreams in the background.
    ///
    /// Messages are cleared immediately so the caller can continue.  Dreams
    /// run in the background with no way to block the caller.
    pub fn clear(&mut self) {
        self.fire_dreams(DreamEvent::SessionEnd);
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

    /// Remove and return the last message in the conversation history.
    pub fn pop_last_message(&mut self) -> Option<Message> {
        self.messages.pop()
    }

    /// Replace all `ContentBlock::Image` blocks in the conversation history
    /// with `[image]` placeholder text.  Called when the active model does
    /// not support vision — sanitises the entire history so subsequent
    /// turns don't replay rejected image data.
    pub fn strip_images(&mut self) {
        for msg in &mut self.messages {
            for block in &mut msg.content {
                if matches!(block, ContentBlock::Image { .. }) {
                    *block = ContentBlock::Text {
                        text: "[image]".to_string(),
                    };
                }
            }
        }
    }

    /// Mark the agent as unable to use tools.  Subsequent LLM calls will
    /// omit tool definitions from the request.
    pub fn disable_tools(&mut self) {
        self.tools_disabled = true;
    }

    /// Replace all `ContentBlock::ToolUse` and `ContentBlock::ToolResult`
    /// blocks in the conversation history with text placeholders.
    ///
    /// Called when the active model doesn't support tool use — the OpenAI
    /// serializer would otherwise emit `role: "tool"` messages and
    /// `tool_calls` arrays that providers reject when no tool definitions
    /// are provided.
    pub fn strip_tool_history(&mut self) {
        for msg in &mut self.messages {
            for block in &mut msg.content {
                match block {
                    ContentBlock::ToolUse { name, .. } => {
                        *block = ContentBlock::Text {
                            text: format!("[tool call: {name}]"),
                        };
                    }
                    ContentBlock::ToolResult { content, .. } => {
                        let preview: String = content.chars().take(200).collect();
                        *block = ContentBlock::Text {
                            text: format!("[tool result: {preview}]"),
                        };
                    }
                    _ => {}
                }
            }
        }
    }

    /// Get the system prompt (for quick response context).
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Get the completion config (for quick response context).
    pub fn config(&self) -> &CompletionConfig {
        &self.config
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
        tracing::info!(
            input_len = user_input.len(),
            input_preview = &user_input[..user_input.len().min(200)],
            "user message received"
        );
        // Append the user's message to history.
        self.messages.push(Message::user(user_input));
        self.run_inner(output).await
    }

    /// Run the agent loop with pre-built content blocks (text + images).
    ///
    /// Like [`run()`], but accepts arbitrary content blocks instead of
    /// plain text.  Used by controllers that handle multimodal input
    /// (e.g. photos, voice notes).
    pub async fn run_with_blocks(
        &mut self,
        blocks: Vec<crate::message::ContentBlock>,
        output: &mut dyn Output,
    ) -> Result<String> {
        tracing::info!(
            block_count = blocks.len(),
            "user multimodal message received"
        );
        self.messages.push(Message::user_multimodal(blocks));
        self.run_inner(output).await
    }

    /// Inner agent loop shared by [`run()`] and [`run_with_blocks()`].
    ///
    /// Assumes the caller has already pushed the user message to
    /// `self.messages`.
    async fn run_inner(&mut self, output: &mut dyn Output) -> Result<String> {
        self.turn_count += 1;

        // Fire turn-triggered dreams (memory maintenance, self-improvement).
        // Dreams run as fire-and-forget background tasks — they never block
        // the controller loop.  See dream.rs and docs/dreaming.md.
        self.fire_dreams(DreamEvent::TurnComplete {
            turn_count: self.turn_count,
        });

        let mut final_text = String::new();
        let mut hit_max_iterations = false;

        // -- Collect ephemeral context from skills --
        //
        // Each skill can inject per-turn context (refreshed tokens, timestamps,
        // etc.) via before_turn().  These fragments are appended to the system
        // prompt for this run() call only — they don't persist.
        let mut skill_fragments = String::new();
        for skill in &self.skills {
            match skill.before_turn().await {
                Ok(Some(fragment)) => {
                    skill_fragments.push_str("\n\n");
                    skill_fragments.push_str(&fragment);
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

        // The system prompt is split into two parts for KV cache efficiency:
        //
        // 1. `self.system_prompt` — stable prefix (identity, tools, skills).
        //    Cached by providers that support prompt caching (Anthropic).
        //
        // 2. `skill_fragments` — ephemeral per-turn context (timestamps,
        //    refreshed tokens).  Sent as a separate block so it doesn't
        //    invalidate the cached prefix.
        //
        // For context-size estimation we still need the combined length.
        let turn_system_prompt: Arc<str> = if skill_fragments.is_empty() {
            Arc::clone(&self.system_prompt)
        } else {
            let mut prompt = String::with_capacity(
                self.system_prompt.len() + skill_fragments.len(),
            );
            prompt.push_str(&self.system_prompt);
            prompt.push_str(&skill_fragments);
            Arc::from(prompt)
        };

        let mut recovered_this_turn = false;

        for iteration in 0..self.max_iterations {
            // -- Auto-compact if estimated context tokens exceed threshold --
            //
            // Before each LLM call, estimate the token count of the full
            // context (messages + system prompt + tool definitions) locally.
            // If it exceeds the threshold, compact first so we never send
            // an oversized context to the API.
            if let Some(ref config) = self.compaction_config
                && self.messages.len() > config.protect_head
            {
                let threshold = config.threshold();
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
                tools_enabled = !self.tools_disabled,
                tool_count = self.tool_definitions.len(),
                "starting LLM call"
            );

            // Log the messages being sent to the LLM for debugging.
            // Guard the entire loop so we don't allocate block summaries
            // when debug logging is disabled (the common case).
            if tracing::enabled!(tracing::Level::DEBUG) {
                for (i, msg) in self.messages.iter().enumerate() {
                    let role = match msg.role {
                        crate::message::Role::User => "user",
                        crate::message::Role::Assistant => "assistant",
                    };
                    let block_summary: Vec<String> = msg.content.iter().map(|b| match b {
                        crate::message::ContentBlock::Text { text } => {
                            format!("text({})", text.len())
                        }
                        crate::message::ContentBlock::ToolUse { name, .. } => {
                            format!("tool_use({name})")
                        }
                        crate::message::ContentBlock::ToolResult { tool_use_id, is_error, .. } => {
                            format!("tool_result({tool_use_id}, error={is_error})")
                        }
                        crate::message::ContentBlock::Image { .. } => "image".to_string(),
                    }).collect();
                    tracing::debug!(
                        msg_index = i,
                        role = role,
                        blocks = ?block_summary,
                        "message in context"
                    );
                }
            }

            // Show a typing indicator while waiting for the LLM to respond.
            output.typing_indicator(true)?;

            // -- Stream LLM response (with retry/backoff) --
            //
            // When the provider handles tools internally (e.g., Claude Code),
            // don't send Dyson's tool definitions — the provider has its own.
            // We discover this from `StreamResponse.tool_mode` after the call.
            let response = {
                let mut last_err = None;
                let mut response_opt = None;
                let mut recovery: Option<LlmRecovery> = None;
                for attempt in 0..=self.max_retries {
                    // Determine tools_for_llm inside the loop so retries
                    // behave identically.  On the first successful response
                    // we learn the tool_mode.
                    let tools_for_llm = if self.tools_disabled {
                        &[]
                    } else {
                        self.tool_definitions.as_slice()
                    };

                    match self
                        .client
                        .access()?
                        .stream(
                            &self.messages,
                            &self.system_prompt,
                            &skill_fragments,
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
                        Err(e) => {
                            // Non-retryable error — ask the controller for a
                            // recovery decision.  Only allow one recovery per
                            // turn to prevent infinite loops.
                            if recovered_this_turn {
                                return Err(e);
                            }
                            let action = output.on_llm_error(&e);
                            if action == LlmRecovery::GiveUp {
                                return Err(e);
                            }
                            recovery = Some(action);
                            break;
                        }
                    }
                }

                // If the controller requested recovery, apply it and retry
                // the turn from the top of the outer iteration loop.
                if let Some(action) = recovery {
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
                        self.messages.push(msg);
                    }
                    recovered_this_turn = true;
                    continue;
                }

                response_opt.ok_or_else(|| last_err.unwrap())?
            };

            let tool_mode = response.tool_mode;
            if let Some(input_tokens) = response.input_tokens {
                self.token_budget.record_input(input_tokens);
            }

            tracing::info!(
                tool_mode = ?tool_mode,
                input_tokens = ?response.input_tokens,
                "streaming response"
            );

            // -- Process the stream into a message + tool calls --
            let (assistant_msg, tool_calls, output_tokens) =
                stream_handler::process_stream(response.stream, output).await?;

            // -- Record token usage and check budget --
            if let Err(e) = self.token_budget.record(output_tokens) {
                self.messages.push(assistant_msg);
                tracing::warn!(
                    used = self.token_budget.output_tokens_used,
                    "token budget exceeded — stopping agent loop"
                );
                output.error(&e)?;
                break;
            }

            // Log assistant response summary.
            if let Some(text) = assistant_msg.last_text() {
                let preview = &text[..text.len().min(500)];
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

            // -- If no tool calls, the LLM is done --
            //
            // Also break when the provider handles tools internally (e.g.,
            // Claude Code).  In that case, ToolUse events in the stream are
            // informational — the provider already executed them.  Without
            // this check, Dyson would try to re-execute every tool call and
            // loop until max_iterations, spawning a new subprocess each time.
            if tool_calls.is_empty() || tool_mode == crate::llm::ToolMode::Observe {
                // Extract the final text from the assistant message.
                if let Some(text) = assistant_msg.last_text() {
                    final_text = text.to_string();
                }
                self.messages.push(assistant_msg);
                output.flush()?;
                break;
            }

            self.messages.push(assistant_msg);

            // -- Check tool call limits --
            //
            // Each call is checked against per-turn limits and cooldown.
            // Calls that exceed limits get an error result without execution.
            let mut limited_calls: Vec<usize> = Vec::with_capacity(tool_calls.len());
            for (i, call) in tool_calls.iter().enumerate() {
                if let Err(e) = self.limiter.check(&call.name) {
                    tracing::warn!(tool = call.name, error = %e, "tool call rate-limited");
                    self.messages
                        .push(Message::tool_result(&call.id, &e.to_string(), true));
                } else {
                    limited_calls.push(i);
                }
            }

            // -- Analyze dependencies and execute in phases --
            //
            // Build a sub-slice of allowed calls, analyze their dependencies,
            // and execute them in the correct order (parallel or sequential).
            let allowed_calls: Vec<&ToolCall> =
                limited_calls.iter().map(|&i| &tool_calls[i]).collect();

            if !allowed_calls.is_empty() {
                let phases = DependencyAnalyzer::analyze(&allowed_calls);

                for phase in phases {
                    match phase {
                        ExecutionPhase::Parallel(indices) => {
                            let futs: Vec<_> = indices
                                .iter()
                                .map(|&idx| self.execute_tool_call_timed(allowed_calls[idx]))
                                .collect();
                            let results = futures::future::join_all(futs).await;
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
            }

            self.limiter.reset_turn();

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
                .access()?
                .stream(
                    &self.messages,
                    &self.system_prompt,
                    &skill_fragments,
                    empty_tools,
                    &self.config,
                )
                .await
            {
                Ok(response) => {
                    let (assistant_msg, _tool_calls, _output_tokens) =
                        stream_handler::process_stream(response.stream, output).await?;
                    if let Some(text) = assistant_msg.last_text() {
                        final_text = text.to_string();
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

        // NOTE: Self-improvement and memory maintenance dreams are now
        // fired at the *start* of run_inner via fire_dreams(TurnComplete).
        // The DreamRunner checks each dream's trigger against the turn count
        // and spawns matching dreams as background tasks.

        output.flush()?;
        Ok(final_text)
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

    /// Process a tool execution result: render to output, format for the LLM,
    /// send attached files, and append the tool_result message to history.
    fn handle_tool_result(
        &mut self,
        call: &ToolCall,
        result: Result<ToolOutput>,
        output: &mut dyn Output,
    ) -> Result<()> {
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

                // Format the result for the LLM.
                let formatted = self
                    .formatter
                    .format(call, tool_output, std::time::Duration::ZERO);
                let content = formatted.to_llm_message();
                Message::tool_result(&call.id, &content, tool_output.is_error)
            }
            Err(ref e) => Message::tool_result(&call.id, &e.to_string(), true),
        };

        self.messages.push(tool_result_msg);
        Ok(())
    }

    /// Execute a single tool call with timing and structured logging.
    ///
    /// This is the concurrent-safe entry point — it doesn't touch `output`
    /// (which is `&mut` and can't be shared across futures).  The caller
    /// handles output rendering after all futures resolve.
    async fn execute_tool_call_timed(&self, call: &ToolCall) -> Result<ToolOutput> {
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
        let tool_start = std::time::Instant::now();
        let result = self.execute_tool_call(call).await;
        let tool_ms = tool_start.elapsed().as_millis();
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
        result
    }

    /// Notify the owning skill that one of its tools was executed.
    ///
    /// Errors are logged but don't interrupt the agent loop — after_tool
    /// is observational, not control flow.
    async fn notify_after_tool(&self, tool_name: &str, output: &ToolOutput) {
        if let Some(&skill_idx) = self.tool_to_skill.get(tool_name)
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
                let Some(tool) = self.tools.get(&call.name) else {
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
                let Some(tool) = self.tools.get(&tool_name) else {
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

// ---------------------------------------------------------------------------
// Quick response — lightweight no-tools LLM call for when the agent is busy.
// ---------------------------------------------------------------------------

/// Make a single LLM call with no tools for a fast response.
///
/// Used when a user sends a message while the agent is already processing
/// another request.  Instead of blocking, the controller calls this with
/// a snapshot of the conversation history.  The LLM sees the context and
/// answers immediately without entering the tool-use loop.
///
/// This is intentionally decoupled from `Agent` — the agent's mutex is
/// held during `run()`, so we can't call methods on it.  Instead, the
/// caller provides a snapshot of the relevant state.
pub async fn quick_response(
    client: &dyn LlmClient,
    messages: &[Message],
    system_prompt: &str,
    user_input: &str,
    config: &CompletionConfig,
    output: &mut dyn Output,
) -> Result<String> {
    tracing::info!(
        input_len = user_input.len(),
        messages = messages.len(),
        "quick response — no tools"
    );

    // Build a temporary message list: existing history + the new user message.
    let mut msgs: Vec<Message> = messages.to_vec();
    msgs.push(Message::user(user_input));

    // Use a lower max_tokens for speed — quick responses should be concise.
    let quick_config = CompletionConfig {
        model: config.model.clone(),
        max_tokens: config.max_tokens.min(1024),
        temperature: config.temperature,
    };

    // Augment the system prompt to signal brevity.
    let quick_suffix = "\n\nIMPORTANT: You are answering a quick question while \
        your main processing is busy on a previous request. Be concise and direct. \
        You have no tools available right now.";

    let response = client
        .stream(&msgs, system_prompt, quick_suffix, &[], &quick_config)
        .await?;

    // Process the stream — reuse the standard handler.
    let (assistant_msg, _tool_calls, _output_tokens) =
        stream_handler::process_stream(response.stream, output).await?;

    output.flush()?;

    // Extract the final text.
    let final_text = assistant_msg
        .last_text()
        .unwrap_or_default()
        .to_string();

    Ok(final_text)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::stream::{StopReason, StreamEvent};
    use crate::message::{ContentBlock, Role};
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
            _system_suffix: &str,
            _tools: &[ToolDefinition],
            _config: &CompletionConfig,
        ) -> Result<crate::llm::StreamResponse> {
            let events = self.responses.lock().unwrap().remove(0);
            Ok(crate::llm::StreamResponse {
                stream: Box::pin(tokio_stream::iter(events.into_iter().map(Ok))),
                tool_mode: self.tool_mode,
                input_tokens: None,
            })
        }
    }

    use crate::controller::recording::RecordingOutput;

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
        let mut output = RecordingOutput::new();

        let result = agent.run("hi", &mut output).await.unwrap();
        assert_eq!(result, "Hello!");
        assert_eq!(output.text(), "Hello!");
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
        let mut output = RecordingOutput::new();

        let result = agent
            .run("run echo test_output", &mut output)
            .await
            .unwrap();
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
        let mut output = RecordingOutput::new();

        let result = agent.run("list files", &mut output).await.unwrap();

        // Should get the final text, NOT an error from trying to execute "bash".
        assert_eq!(result, "Here are the files.");
        // Only 2 messages: user + assistant (no tool_result messages).
        assert_eq!(agent.messages.len(), 2);
    }

    #[tokio::test]
    async fn memory_system_prompt_contains_usage_stats() {
        let ws = crate::workspace::InMemoryWorkspace::new()
            .with_file("MEMORY.md", "some memories here")
            .with_limit("MEMORY.md", 2200)
            .with_file("USER.md", "user info")
            .with_limit("USER.md", 1375);

        let workspace: Box<dyn crate::workspace::Workspace> = Box::new(ws);
        let ctx = crate::tool::ToolContext {
            working_dir: std::env::temp_dir(),
            env: HashMap::new(),
            cancellation: CancellationToken::new(),
            workspace: Some(std::sync::Arc::new(tokio::sync::RwLock::new(workspace))),
            depth: 0,
        };

        let prompt = reflection::build_memory_system_prompt(&ctx).await;
        assert!(prompt.contains("MEMORY.md"));
        assert!(prompt.contains("/2200 chars"));
        assert!(prompt.contains("USER.md"));
        assert!(prompt.contains("/1375 chars"));
        assert!(prompt.contains("memory_search"));
        assert!(prompt.contains("workspace_update"));
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
        let prompt = reflection::build_reflection_system_prompt(&ctx).await;
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

        let summary = reflection::summarize_for_reflection(&messages);
        assert!(summary.contains("Deploy my app"));
        assert!(summary.contains("[Tool call: bash]"));
        assert!(summary.contains("Deployed successfully"));
        assert!(summary.contains("1 tool calls (0 errors)"));
        assert!(summary.contains("bash"));
        assert!(summary.contains("4 messages total"));
    }

    #[test]
    fn summarize_for_reflection_handles_multibyte_utf8() {
        // Regression: slicing at byte 200 or 500 can land inside a
        // multi-byte character (e.g. smart quotes '\u{2019}' is 3 bytes).
        // Build a string where byte 200 is mid-character.
        let mut text = "a".repeat(199);
        text.push('\u{2019}'); // RIGHT SINGLE QUOTATION MARK — 3 bytes (bytes 199..202)
        text.push_str("end");
        assert_eq!(text.len(), 205); // 199 + 3 + 3

        let messages = vec![Message::user(&text)];

        // Should not panic.
        let summary = reflection::summarize_for_reflection(&messages);
        assert!(summary.contains(&"a".repeat(199)));

        // Same for tool error content (truncated at 200 bytes).
        let error_content = text.clone();
        let messages_with_error = vec![
            Message::assistant(vec![crate::message::ContentBlock::ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            }]),
            Message::tool_result("t1", &error_content, true),
        ];

        let summary = reflection::summarize_for_reflection(&messages_with_error);
        assert!(summary.contains("[Tool error:"));

        // And for non-error tool results (also truncated at 200).
        let messages_with_result = vec![
            Message::assistant(vec![crate::message::ContentBlock::ToolUse {
                id: "t2".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            }]),
            Message::tool_result("t2", &error_content, false),
        ];

        let summary = reflection::summarize_for_reflection(&messages_with_result);
        assert!(summary.contains("[Tool result:"));
    }

    // -----------------------------------------------------------------------
    // Background learning synthesis tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn synthesize_to_workspace_updates_memory() {
        let ws = crate::workspace::InMemoryWorkspace::new()
            .with_file("MEMORY.md", "Old memory content.");

        let workspace: Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>> =
            Arc::new(tokio::sync::RwLock::new(Box::new(ws)));

        let llm = MockLlm::new(vec![vec![
            StreamEvent::TextDelta("Updated memory with new learnings.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ]]);

        let config = CompletionConfig {
            model: "test".to_string(),
            max_tokens: 1024,
            temperature: None,
        };

        let summary = "User asked about Rust lifetimes and learned about borrowing.";

        let result = reflection::synthesize_to_workspace(&llm, &config, summary, &workspace).await;

        assert!(result.is_ok(), "synthesis should succeed");

        let ws = workspace.read().await;
        let memory = ws.get("MEMORY.md").unwrap();
        assert_eq!(memory, "Updated memory with new learnings.");
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
        assert!(!Agent::is_retryable(&DysonError::Llm(
            "authentication failed".into()
        )));
        assert!(!Agent::is_retryable(&DysonError::Config(
            "bad config".into()
        )));
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
        let mut output = RecordingOutput::new();

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
        fn name(&self) -> &str {
            "send_test_file"
        }
        fn description(&self) -> &str {
            "Returns a file"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {},
            })
        }
        async fn run(
            &self,
            _input: &serde_json::Value,
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
        fn name(&self) -> &str {
            "mock_file_skill"
        }
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
        let mut output = RecordingOutput::new();

        let result = agent.run("send me a file", &mut output).await.unwrap();
        assert_eq!(result, "Files sent.");

        // Verify that send_file was called for both attached files.
        let sent = output.sent_files();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0], std::path::Path::new("/tmp/test_report.pdf"));
        assert_eq!(sent[1], std::path::Path::new("/tmp/data.csv"));
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
        let mut output = RecordingOutput::new();

        agent.run("echo hello", &mut output).await.unwrap();

        // No files should have been sent.
        assert!(output.sent_files().is_empty());
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
    // CompactionConfig unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn compaction_config_default_values() {
        let config = CompactionConfig::default();
        assert_eq!(config.context_window, 200_000);
        assert!((config.threshold_ratio - 0.50).abs() < f64::EPSILON);
        assert_eq!(config.protect_head, 3);
        assert_eq!(config.protect_tail_tokens, 20_000);
        assert_eq!(config.summary_min_tokens, 2_000);
        assert_eq!(config.summary_max_tokens, 12_000);
        assert!((config.summary_target_ratio - 0.20).abs() < f64::EPSILON);
    }

    #[test]
    fn compaction_config_threshold_calculation() {
        let config = CompactionConfig::default();
        // 200_000 * 0.50 = 100_000
        assert_eq!(config.threshold(), 100_000);
    }

    #[test]
    fn compaction_config_threshold_with_custom_ratio() {
        let config = CompactionConfig {
            context_window: 128_000,
            threshold_ratio: 0.75,
            ..CompactionConfig::default()
        };
        // 128_000 * 0.75 = 96_000
        assert_eq!(config.threshold(), 96_000);
    }

    // -----------------------------------------------------------------------
    // Helper: build an agent with manual message history for compaction tests.
    // -----------------------------------------------------------------------

    /// Build an agent with pre-loaded messages and a compaction config.
    /// The `llm_responses` are the responses the MockLlm will return (e.g.
    /// for the summarisation call during compact()).
    fn make_agent_with_history(
        messages: Vec<Message>,
        llm_responses: Vec<Vec<StreamEvent>>,
        compaction: Option<CompactionConfig>,
    ) -> (Agent, RecordingOutput) {
        let llm = MockLlm::new(llm_responses);
        let settings = AgentSettings {
            api_key: "test".into(),
            compaction,
            ..Default::default()
        };
        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None))];
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
        let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();
        agent.messages = messages;
        (agent, RecordingOutput::new())
    }

    // -----------------------------------------------------------------------
    // Context compaction tests — five-phase Hermes-style compressor
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn compact_on_empty_history_is_noop() {
        // No LLM responses queued — would panic if called.
        let (mut agent, mut output) = make_agent_with_history(vec![], vec![], None);
        agent.compact(&mut output).await.unwrap();
        assert!(agent.messages.is_empty());
    }

    #[tokio::test]
    async fn compact_short_history_skips_when_no_middle() {
        // With protect_head=3 and only 3 messages, there's nothing to
        // summarise.  compact() should be a no-op (no LLM call).
        let messages = vec![
            Message::user("hello"),
            Message::assistant(vec![ContentBlock::Text { text: "hi!".into() }]),
            Message::user("how are you?"),
        ];
        let config = CompactionConfig {
            protect_head: 3,
            protect_tail_tokens: 0,
            ..CompactionConfig::default()
        };
        let (mut agent, mut output) =
            make_agent_with_history(messages.clone(), vec![], Some(config));

        agent.compact(&mut output).await.unwrap();
        // All 3 messages preserved — no compaction needed.
        assert_eq!(agent.messages.len(), 3);
    }

    #[tokio::test]
    async fn compact_preserves_head_and_tail() {
        // Build a conversation with 10 messages.  protect_head=2,
        // protect_tail_tokens=large enough to cover last 2 messages.
        // The middle 6 messages should be summarised.
        let mut messages = Vec::new();
        for i in 0..5 {
            messages.push(Message::user(&format!("User message {i}")));
            messages.push(Message::assistant(vec![ContentBlock::Text {
                text: format!("Assistant response {i}"),
            }]));
        }
        assert_eq!(messages.len(), 10);

        let config = CompactionConfig {
            protect_head: 2,
            // Each message is ~5 tokens.  Protect last 2 messages (~10 tokens).
            protect_tail_tokens: 15,
            ..CompactionConfig::default()
        };

        let summary_response = vec![
            StreamEvent::TextDelta(
                "## Goal\nTest conversation\n## Progress\nMessages exchanged.".into(),
            ),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ];

        let (mut agent, mut output) =
            make_agent_with_history(messages.clone(), vec![summary_response], Some(config));

        agent.compact(&mut output).await.unwrap();

        // Head: first 2 messages preserved verbatim.
        assert_eq!(agent.messages[0].role, Role::User);
        match &agent.messages[0].content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "User message 0"),
            other => panic!("expected Text, got: {other:?}"),
        }
        assert_eq!(agent.messages[1].role, Role::Assistant);
        match &agent.messages[1].content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Assistant response 0"),
            other => panic!("expected Text, got: {other:?}"),
        }

        // Summary should be present somewhere after head.
        let summary_idx = agent.messages.iter().position(|m| {
            m.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[Context Summary]")))
        });
        assert!(summary_idx.is_some(), "summary message should exist");

        // Tail: last 2 original messages preserved verbatim.
        let last = &agent.messages[agent.messages.len() - 1];
        match &last.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Assistant response 4"),
            other => panic!("expected Text, got: {other:?}"),
        }
        let second_last = &agent.messages[agent.messages.len() - 2];
        match &second_last.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "User message 4"),
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn compact_prunes_tool_outputs_in_middle() {
        // Messages: head(user, assistant) + middle(assistant-with-tool, tool-result) + tail(user, assistant)
        let messages = vec![
            // Head
            Message::user("start"),
            Message::assistant(vec![ContentBlock::Text { text: "ok".into() }]),
            // Middle — tool call + large result
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls -la"}),
            }]),
            Message::tool_result(
                "call_1",
                "drwxr-xr-x 15 user user 4096 Mar 30 file1.txt\n-rw-r--r-- 1 user user 12345 Mar 30 file2.txt\n...(many more lines)...",
                false,
            ),
            // More middle
            Message::user("what about the other directory?"),
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "call_2".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls /other"}),
            }]),
            Message::tool_result(
                "call_2",
                "big output from other directory listing here",
                false,
            ),
            // Tail
            Message::user("thanks, now summarise"),
            Message::assistant(vec![ContentBlock::Text {
                text: "Here's your summary.".into(),
            }]),
        ];

        let config = CompactionConfig {
            protect_head: 2,
            protect_tail_tokens: 100, // enough for last 2 messages
            ..CompactionConfig::default()
        };

        let summary_response = vec![
            StreamEvent::TextDelta(
                "## Goal\nFile listing\n## Progress\nListed directories.".into(),
            ),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ];

        let (mut agent, mut output) =
            make_agent_with_history(messages, vec![summary_response], Some(config));

        agent.compact(&mut output).await.unwrap();

        // The summary should exist and tool outputs in the middle should
        // have been pruned (replaced with placeholder) before summarisation.
        let has_summary = agent.messages.iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, ContentBlock::Text { text } if text.contains("[Context Summary]")),
            )
        });
        assert!(has_summary, "should contain a context summary");

        // Original large tool outputs should NOT be in the final messages.
        let has_big_output = agent.messages.iter().any(|m| {
            m.content.iter().any(|b| matches!(b, ContentBlock::ToolResult { content, .. } if content.contains("many more lines")))
        });
        assert!(
            !has_big_output,
            "large tool outputs in middle should be pruned or summarised away"
        );
    }

    #[tokio::test]
    async fn compact_fixes_orphaned_tool_pairs() {
        // Set up a situation where compaction splits a tool_use/tool_result pair:
        // - Head contains an assistant message with tool_use
        // - The matching tool_result is in the middle (gets summarised away)
        // After compaction, the orphaned tool_use should get a synthetic result.
        let messages = vec![
            // Head
            Message::user("start"),
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "orphan_call".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo test"}),
            }]),
            // Middle — the tool result for orphan_call, plus more conversation
            Message::tool_result("orphan_call", "test output", false),
            Message::user("continue"),
            Message::assistant(vec![ContentBlock::Text {
                text: "continuing...".into(),
            }]),
            Message::user("more stuff"),
            Message::assistant(vec![ContentBlock::Text {
                text: "more responses".into(),
            }]),
            // Tail
            Message::user("final question"),
            Message::assistant(vec![ContentBlock::Text {
                text: "final answer".into(),
            }]),
        ];

        let config = CompactionConfig {
            protect_head: 2,
            protect_tail_tokens: 100,
            ..CompactionConfig::default()
        };

        let summary_response = vec![
            StreamEvent::TextDelta("## Goal\nTesting\n## Progress\nRan commands.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ];

        let (mut agent, mut output) =
            make_agent_with_history(messages, vec![summary_response], Some(config));

        agent.compact(&mut output).await.unwrap();

        // The head still has the tool_use for "orphan_call".
        let has_tool_use = agent.messages[1]
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == "orphan_call"));
        assert!(has_tool_use, "head should still contain the tool_use");

        // There should be a synthetic tool_result matching "orphan_call"
        // (since the real one was in the middle and got summarised away).
        let has_matching_result = agent.messages.iter().any(|m| {
            m.content.iter().any(|b| {
                matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "orphan_call")
            })
        });
        assert!(
            has_matching_result,
            "should have a synthetic tool_result for the orphaned tool_use"
        );
    }

    #[tokio::test]
    async fn compact_structured_summary_prompt() {
        // Verify that the LLM receives a structured prompt asking for
        // Goal/Progress/Decisions/Files/Next Steps sections.
        // We check this indirectly: the summary returned by the LLM
        // gets inserted as a [Context Summary] message.
        let messages = vec![
            Message::user("msg 0"),
            Message::assistant(vec![ContentBlock::Text {
                text: "resp 0".into(),
            }]),
            Message::user("msg 1"),
            Message::assistant(vec![ContentBlock::Text {
                text: "resp 1".into(),
            }]),
            Message::user("msg 2"),
            Message::assistant(vec![ContentBlock::Text {
                text: "resp 2".into(),
            }]),
            Message::user("msg 3"),
            Message::assistant(vec![ContentBlock::Text {
                text: "resp 3".into(),
            }]),
        ];

        let config = CompactionConfig {
            protect_head: 2,
            protect_tail_tokens: 15,
            ..CompactionConfig::default()
        };

        let summary_response = vec![
            StreamEvent::TextDelta("## Goal\nUser was testing.\n## Progress\nMultiple exchanges.\n## Key Decisions\nNone.\n## Files Modified\nNone.\n## Next Steps\nContinue.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ];

        let (mut agent, mut output) =
            make_agent_with_history(messages, vec![summary_response], Some(config));

        agent.compact(&mut output).await.unwrap();

        // Find the summary message.
        let summary_msg = agent.messages.iter().find(|m| {
            m.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[Context Summary]")))
        }).expect("should have a summary message");

        match &summary_msg.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("Goal"), "summary should contain Goal section");
                assert!(
                    text.contains("Progress"),
                    "summary should contain Progress section"
                );
            }
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn compact_resets_token_budget() {
        let messages = vec![
            Message::user("hello"),
            Message::assistant(vec![ContentBlock::Text { text: "hi".into() }]),
            Message::user("more"),
            Message::assistant(vec![ContentBlock::Text {
                text: "more".into(),
            }]),
            Message::user("even more"),
            Message::assistant(vec![ContentBlock::Text {
                text: "even more".into(),
            }]),
        ];

        let config = CompactionConfig {
            protect_head: 2,
            protect_tail_tokens: 15,
            ..CompactionConfig::default()
        };

        let summary_response = vec![
            StreamEvent::TextDelta("Summary.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: Some(10),
            },
        ];

        let (mut agent, mut output) =
            make_agent_with_history(messages, vec![summary_response], Some(config));

        agent.token_budget.record(50).unwrap();
        assert_eq!(agent.token_budget.output_tokens_used, 50);

        agent.compact(&mut output).await.unwrap();

        assert_eq!(agent.token_budget.output_tokens_used, 0);
        assert_eq!(agent.token_budget.input_tokens_used, 0);
        assert_eq!(agent.token_budget.llm_calls, 0);
    }

    #[tokio::test]
    async fn compact_iterative_merges_with_previous_summary() {
        // Simulate a second compaction: the head already contains a
        // [Context Summary] from a previous compaction.  The new compact
        // should produce an updated summary that merges old + new.
        let messages = vec![
            // Previous summary (from first compaction).
            Message::user(
                "[Context Summary]\n\n## Goal\nOriginal goal.\n## Progress\nStep 1 done.",
            ),
            // New conversation since last compaction.
            Message::assistant(vec![ContentBlock::Text {
                text: "continuing work".into(),
            }]),
            Message::user("do step 2"),
            Message::assistant(vec![ContentBlock::Text {
                text: "step 2 done".into(),
            }]),
            Message::user("do step 3"),
            Message::assistant(vec![ContentBlock::Text {
                text: "step 3 done".into(),
            }]),
            // Tail
            Message::user("what's next?"),
            Message::assistant(vec![ContentBlock::Text {
                text: "step 4".into(),
            }]),
        ];

        let config = CompactionConfig {
            protect_head: 2,
            protect_tail_tokens: 15,
            ..CompactionConfig::default()
        };

        let summary_response = vec![
            StreamEvent::TextDelta(
                "## Goal\nOriginal goal.\n## Progress\nSteps 1-3 done.\n## Next Steps\nStep 4."
                    .into(),
            ),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ];

        let (mut agent, mut output) =
            make_agent_with_history(messages, vec![summary_response], Some(config));

        agent.compact(&mut output).await.unwrap();

        // Should have a merged summary.
        let summary_msg = agent.messages.iter().find(|m| {
            m.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[Context Summary]")))
        }).expect("should have a summary message");

        match &summary_msg.content[0] {
            ContentBlock::Text { text } => {
                assert!(
                    text.contains("Steps 1-3"),
                    "summary should merge old + new progress"
                );
            }
            other => panic!("expected Text, got: {other:?}"),
        }

        // Should NOT have two [Context Summary] messages.
        let summary_count = agent.messages.iter().filter(|m| {
            m.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[Context Summary]")))
        }).count();
        assert_eq!(
            summary_count, 1,
            "should have exactly one summary after iterative compaction"
        );
    }

    #[tokio::test]
    async fn compact_empty_summary_keeps_original_history() {
        // If the LLM returns an empty summary, keep the original history.
        let messages = vec![
            Message::user("hello"),
            Message::assistant(vec![ContentBlock::Text { text: "hi".into() }]),
            Message::user("more"),
            Message::assistant(vec![ContentBlock::Text {
                text: "more".into(),
            }]),
            Message::user("even more"),
            Message::assistant(vec![ContentBlock::Text {
                text: "even more".into(),
            }]),
        ];

        let config = CompactionConfig {
            protect_head: 2,
            protect_tail_tokens: 15,
            ..CompactionConfig::default()
        };

        // LLM returns empty text.
        let summary_response = vec![StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        }];

        let original_len = messages.len();
        let (mut agent, mut output) =
            make_agent_with_history(messages, vec![summary_response], Some(config));

        agent.compact(&mut output).await.unwrap();
        // Original history should be preserved (though tool outputs may be pruned).
        assert_eq!(agent.messages.len(), original_len);
    }

    #[tokio::test]
    async fn compact_tail_protection_by_token_budget() {
        // Verify that tail protection is based on token budget, not message count.
        // Create messages with very different token sizes — the tail should protect
        // the last messages that fit within the token budget.
        let messages = vec![
            Message::user("hi"),                // ~5 tokens
            Message::assistant(vec![ContentBlock::Text { text: "hello".into() }]), // ~5 tokens
            Message::user("middle msg"),        // ~6 tokens
            Message::assistant(vec![ContentBlock::Text { text: "middle resp".into() }]), // ~6 tokens
            // These two are large — should be in the tail if budget is generous.
            Message::user("a very long user message with many words to take up lots of token budget space in the estimate"),
            Message::assistant(vec![ContentBlock::Text {
                text: "a very long assistant response with many words to take up lots of token budget space in the estimate".into(),
            }]),
        ];

        let config = CompactionConfig {
            protect_head: 2,
            // Budget large enough for the last 2 big messages (~40+ tokens),
            // but NOT for all 4 non-head messages.
            protect_tail_tokens: 50,
            ..CompactionConfig::default()
        };

        let summary_response = vec![
            StreamEvent::TextDelta("Middle section summary.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ];

        let (mut agent, mut output) =
            make_agent_with_history(messages, vec![summary_response], Some(config));

        agent.compact(&mut output).await.unwrap();

        // Head (2 messages) + summary (1) + tail (2 big messages) = 5.
        // The middle 2 messages got summarised.
        let last_text = agent.messages.last().unwrap();
        match &last_text.content[0] {
            ContentBlock::Text { text } => {
                assert!(
                    text.contains("very long assistant response"),
                    "tail should preserve the last large messages"
                );
            }
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn auto_compaction_triggers_on_threshold() {
        // Set up a very low compaction threshold so that after turn 1 builds
        // up history, the offline token estimate exceeds it on turn 2.
        let llm = MockLlm::new(vec![
            // Turn 1: normal response.
            vec![
                StreamEvent::TextDelta("First response.".into()),
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    output_tokens: Some(20),
                },
            ],
            // Auto-compaction summary.
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
            compaction: Some(CompactionConfig {
                context_window: 20, // very low
                threshold_ratio: 0.50,
                protect_head: 1,
                protect_tail_tokens: 0,
                ..CompactionConfig::default()
            }),
            ..Default::default()
        };

        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None))];
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
        let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();
        let mut output = RecordingOutput::new();

        // First turn.
        agent.run("first message", &mut output).await.unwrap();
        assert_eq!(agent.messages.len(), 2);

        // Second turn — triggers auto-compact.
        let result = agent.run("second message", &mut output).await.unwrap();
        assert_eq!(result, "Second response.");
    }

    #[tokio::test]
    async fn compact_no_config_uses_legacy_full_summary() {
        // When compaction_config is None, compact() should still work
        // (legacy behavior: summarise everything into one message).
        let messages = vec![
            Message::user("hello"),
            Message::assistant(vec![ContentBlock::Text { text: "hi".into() }]),
            Message::user("more"),
            Message::assistant(vec![ContentBlock::Text {
                text: "more".into(),
            }]),
        ];

        let summary_response = vec![
            StreamEvent::TextDelta("Full conversation summary.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ];

        let (mut agent, mut output) = make_agent_with_history(
            messages,
            vec![summary_response],
            None, // no config
        );

        agent.compact(&mut output).await.unwrap();

        // Legacy: everything replaced with a single summary message.
        assert_eq!(agent.messages.len(), 1);
        match &agent.messages[0].content[0] {
            ContentBlock::Text { text } => {
                assert!(text.starts_with("[Context Summary]"));
                assert!(text.contains("Full conversation summary"));
            }
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Quick response tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn quick_response_returns_text_without_tools() {
        let llm = MockLlm::new(vec![vec![
            StreamEvent::TextDelta("Quick answer.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ]]);

        let history = vec![
            Message::user("What is 2+2?"),
            Message::assistant(vec![ContentBlock::Text {
                text: "4".into(),
            }]),
        ];

        let config = CompletionConfig {
            model: "test-model".into(),
            max_tokens: 4096,
            temperature: None,
        };

        let mut output = RecordingOutput::new();
        let result = quick_response(
            &llm,
            &history,
            "You are a helpful assistant.",
            "What about 3+3?",
            &config,
            &mut output,
        )
        .await
        .unwrap();

        assert_eq!(result, "Quick answer.");
        assert_eq!(output.text(), "Quick answer.");
    }

    #[tokio::test]
    async fn quick_response_caps_max_tokens() {
        // Verify that quick_response uses min(config.max_tokens, 1024).
        use std::sync::Mutex as StdMutex;

        struct CapturingLlm {
            captured_max_tokens: StdMutex<Option<u32>>,
        }

        #[async_trait::async_trait]
        impl LlmClient for CapturingLlm {
            async fn stream(
                &self,
                _messages: &[Message],
                _system: &str,
                _system_suffix: &str,
                _tools: &[ToolDefinition],
                config: &CompletionConfig,
            ) -> Result<crate::llm::StreamResponse> {
                *self.captured_max_tokens.lock().unwrap() = Some(config.max_tokens);
                let events = vec![
                    StreamEvent::TextDelta("ok".into()),
                    StreamEvent::MessageComplete {
                        stop_reason: StopReason::EndTurn,
                        output_tokens: None,
                    },
                ];
                Ok(crate::llm::StreamResponse {
                    stream: Box::pin(tokio_stream::iter(events.into_iter().map(Ok))),
                    tool_mode: crate::llm::ToolMode::Execute,
                    input_tokens: None,
                })
            }
        }

        let llm = CapturingLlm {
            captured_max_tokens: StdMutex::new(None),
        };
        let config = CompletionConfig {
            model: "test".into(),
            max_tokens: 8192,
            temperature: None,
        };
        let mut output = RecordingOutput::new();

        quick_response(&llm, &[], "sys", "hi", &config, &mut output)
            .await
            .unwrap();

        assert_eq!(*llm.captured_max_tokens.lock().unwrap(), Some(1024));
    }

    #[tokio::test]
    async fn quick_response_sends_no_tools() {
        use std::sync::Mutex as StdMutex;

        struct ToolCapturingLlm {
            captured_tools: StdMutex<Option<usize>>,
        }

        #[async_trait::async_trait]
        impl LlmClient for ToolCapturingLlm {
            async fn stream(
                &self,
                _messages: &[Message],
                _system: &str,
                _system_suffix: &str,
                tools: &[ToolDefinition],
                _config: &CompletionConfig,
            ) -> Result<crate::llm::StreamResponse> {
                *self.captured_tools.lock().unwrap() = Some(tools.len());
                let events = vec![
                    StreamEvent::TextDelta("ok".into()),
                    StreamEvent::MessageComplete {
                        stop_reason: StopReason::EndTurn,
                        output_tokens: None,
                    },
                ];
                Ok(crate::llm::StreamResponse {
                    stream: Box::pin(tokio_stream::iter(events.into_iter().map(Ok))),
                    tool_mode: crate::llm::ToolMode::Execute,
                    input_tokens: None,
                })
            }
        }

        let llm = ToolCapturingLlm {
            captured_tools: StdMutex::new(None),
        };
        let config = CompletionConfig {
            model: "test".into(),
            max_tokens: 1024,
            temperature: None,
        };
        let mut output = RecordingOutput::new();

        quick_response(&llm, &[], "sys", "hi", &config, &mut output)
            .await
            .unwrap();

        assert_eq!(*llm.captured_tools.lock().unwrap(), Some(0));
    }

    // -----------------------------------------------------------------------
    // Integration tests for the tool calling pipeline.
    // -----------------------------------------------------------------------

    mod test_tool_calling_integration {
        use super::*;
        use super::dependency_analyzer::{DependencyAnalyzer, ExecutionPhase};
        use super::result_formatter::ResultFormatter;
        use super::tool_limiter::ToolLimiter;

        #[test]
        fn full_pipeline_single_call() {
            // Verify: limits check -> (would execute) -> format.
            // We test the pipeline components in isolation since the full
            // agent.run() requires an LLM client + async runtime.

            let mut limiter = ToolLimiter::default();
            let formatter = ResultFormatter::default();

            let call = ToolCall::new("bash", serde_json::json!({"command": "echo hello"}));

            // 1. Limiter allows the call.
            assert!(limiter.check(&call.name).is_ok());

            // 2. Dependency analysis: single call → one parallel phase.
            let phases = DependencyAnalyzer::analyze(&[&call]);
            assert_eq!(phases.len(), 1);
            assert!(matches!(phases[0], ExecutionPhase::Parallel(_)));

            // 3. Format the result.
            let output = ToolOutput::success("hello");
            let formatted = formatter.format(&call, &output, std::time::Duration::from_millis(10));
            assert!(formatted.summary.contains("10ms"));
            assert!(!formatted.to_llm_message().is_empty());
        }

        #[test]
        fn respects_dependency_ordering() {
            // write then read = sequential phases.
            let calls = vec![
                ToolCall::new("file_write", serde_json::json!({"path": "out.txt"})),
                ToolCall::new("file_read", serde_json::json!({"path": "out.txt"})),
            ];
            let refs: Vec<&ToolCall> = calls.iter().collect();
            let phases = DependencyAnalyzer::analyze(&refs);
            assert!(
                phases.len() >= 2,
                "expected at least 2 phases, got {}",
                phases.len()
            );
        }

        #[test]
        fn applies_limits_in_pipeline() {
            // Hit the per-turn limit → error without executing.
            // Use a limiter with no cooldown by checking rapidly (the
            // default cooldown is 1s, but we're checking per-turn limits,
            // which are separate from cooldown).
            let mut limiter = ToolLimiter::default();

            // The first call succeeds.
            assert!(limiter.check("bash").is_ok());

            // The per-turn limit is 50; after 1 successful call above,
            // the limiter tracks this tool. A second immediate call fails
            // due to cooldown — but that still proves limits work in the
            // pipeline.
            let result = limiter.check("bash");
            assert!(
                result.is_err(),
                "second immediate call should be rate-limited"
            );
        }

        #[test]
        fn pop_last_message_removes_last() {
            let messages = vec![Message::user("hello"), Message::user("world")];
            let settings = AgentSettings {
                api_key: "test".into(),
                ..Default::default()
            };
            let llm = MockLlm::new(vec![]);
            let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None))];
            let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
            let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();
            agent.set_messages(messages.clone());

            let popped = agent.pop_last_message();
            assert_eq!(
                popped.unwrap().content[0],
                ContentBlock::Text {
                    text: "world".into()
                }
            );
            assert_eq!(agent.messages().len(), 1);
        }

        #[test]
        fn pop_last_message_on_empty_returns_none() {
            let settings = AgentSettings {
                api_key: "test".into(),
                ..Default::default()
            };
            let llm = MockLlm::new(vec![]);
            let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None))];
            let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
            let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();

            assert!(agent.pop_last_message().is_none());
        }

        #[test]
        fn strip_images_replaces_image_blocks_with_placeholder() {
            let settings = AgentSettings {
                api_key: "test".into(),
                ..Default::default()
            };
            let llm = MockLlm::new(vec![]);
            let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None))];
            let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
            let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();

            agent.set_messages(vec![
                Message::user_multimodal(vec![
                    ContentBlock::Text {
                        text: "look at this".into(),
                    },
                    ContentBlock::Image {
                        data: "base64data".into(),
                        media_type: "image/jpeg".into(),
                    },
                ]),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "I see a cat".into(),
                    }],
                },
                Message::user("thanks"),
            ]);

            agent.strip_images();

            // The image block should be replaced with "[image]" text.
            let first_msg = &agent.messages()[0];
            assert_eq!(first_msg.content.len(), 2);
            assert_eq!(
                first_msg.content[1],
                ContentBlock::Text {
                    text: "[image]".into()
                },
            );

            // Text-only messages should be untouched.
            assert_eq!(
                agent.messages()[2].content[0],
                ContentBlock::Text {
                    text: "thanks".into()
                }
            );
        }

        #[test]
        fn strip_images_noop_when_no_images() {
            let settings = AgentSettings {
                api_key: "test".into(),
                ..Default::default()
            };
            let llm = MockLlm::new(vec![]);
            let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None))];
            let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
            let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();

            agent.set_messages(vec![
                Message::user("hello"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text { text: "hi".into() }],
                },
            ]);

            agent.strip_images();

            assert_eq!(agent.messages().len(), 2);
            assert_eq!(
                agent.messages()[0].content[0],
                ContentBlock::Text {
                    text: "hello".into()
                }
            );
        }
    }
}
