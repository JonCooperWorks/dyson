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
//     │  client:  RateLimited<Box<dyn LlmClient>>        │
//     │  sandbox: Arc<dyn Sandbox>     ← gates all calls │
//     │  skills:  Vec<Box<dyn Skill>>                    │
//     │  tool_registry: ToolRegistry   ← immutable tools │
//     │  conversation: Conversation    ← mutable state   │
//     │  system_prompt: Arc<str>                         │
//     │  config: CompletionConfig                        │
//     │  max_iterations: usize                           │
//     │  limiter: ToolLimiter          ← rate limiting   │
//     │  formatter: ResultFormatter    ← output format   │
//     │  history_backend: Option<HistoryBackend>         │
//     └──────────────────────────────────────────────────┘
//
//   Sub-structs group related fields:
//     ToolRegistry  — tools, definitions, cached_tokens, disabled
//     Conversation  — messages, turn_count, token_budget
//     HistoryBackend — store + chat_id (always set together)
//
// Why does Agent own both skills AND a ToolRegistry?
//   Skills own tools (for lifecycle management), but the agent needs O(1)
//   lookup by tool name when dispatching calls.  ToolRegistry provides
//   that.  Both hold Arc<dyn Tool> to the same underlying objects — no
//   duplication, just shared references.
// ===========================================================================

mod compaction;
mod dependency_analyzer;
pub mod dream;
mod execution;
pub mod rate_limiter;
mod reflection;
mod result_formatter;
mod silent_output;
pub mod stream_handler;
pub mod token_budget;
mod tool_limiter;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::chat_history::ChatHistory;
use crate::config::{AgentSettings, CompactionConfig};
use crate::controller::Output;
use crate::error::{DysonError, LlmRecovery, Result};
use crate::llm::{CompletionConfig, LlmClient, ToolDefinition};
use crate::message::{ContentBlock, Message};
use crate::sandbox::Sandbox;
use crate::skill::Skill;
use crate::tool::{Tool, ToolContext};
use self::dream::{DreamEvent, DreamHandle};
use self::result_formatter::ResultFormatter;
use self::tool_limiter::ToolLimiter;

use self::stream_handler::ToolCall;

use self::token_budget::TokenBudget;

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// Result of attempting to stream an LLM response with retry/recovery.
enum StreamResult {
    /// Successful response from the LLM.
    Response(crate::llm::StreamResponse),
    /// Controller requested recovery — caller should `continue` the loop.
    Recovered,
    /// Fatal error — caller should return.
    Error(DysonError),
}

// ---------------------------------------------------------------------------
// Sub-structs — group related Agent fields into focused types.
// ---------------------------------------------------------------------------

/// Immutable tool registry — built once at construction from all skills' tools.
///
/// Provides O(1) tool lookup by name, reverse mapping to owning skill,
/// and tool definitions for LLM requests.  The `tools_disabled` flag
/// controls whether definitions are sent to the LLM (set when the active
/// model doesn't support tool use).
pub(crate) struct ToolRegistry {
    /// Flat tool lookup map: tool_name → Arc<dyn Tool>.
    ///
    /// Shared ownership (Arc) with the skills — no cloning of tool
    /// implementations.
    pub(crate) tools: HashMap<String, Arc<dyn Tool>>,

    /// Reverse index: tool_name → skill index in `Agent::skills`.
    ///
    /// Used to dispatch `after_tool()` to the owning skill.
    tool_to_skill: HashMap<String, usize>,

    /// Tool definitions sent to the LLM so it knows what tools are available.
    definitions: Vec<ToolDefinition>,

    /// Cached sum of estimated tokens for all tool definitions.
    /// Tool definitions are immutable after construction, so this is computed
    /// once and reused in `estimate_context_tokens()`.
    cached_tokens: usize,

    /// When `true`, tool definitions are omitted from LLM requests.
    /// Set when the active model doesn't support tool use.
    disabled: bool,
}

impl ToolRegistry {
    /// Build a tool registry by flattening all skills' tools.
    ///
    /// Duplicate tool names are handled by last-writer-wins (later skills
    /// override earlier ones), with a warning logged.
    fn from_skills(skills: &[Box<dyn Skill>]) -> Self {
        let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        let mut tool_to_skill: HashMap<String, usize> = HashMap::new();
        let mut definitions: Vec<ToolDefinition> = Vec::new();

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

                definitions.push(ToolDefinition {
                    name: name.clone(),
                    description: tool.description().to_string(),
                    input_schema: tool.input_schema(),
                    agent_only: tool.agent_only(),
                });

                tools.insert(name.clone(), Arc::clone(tool));
                tool_to_skill.insert(name, skill_idx);
            }
        }

        let cached_tokens: usize = definitions
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
            "tool registry built"
        );

        Self {
            tools,
            tool_to_skill,
            definitions,
            cached_tokens,
            disabled: false,
        }
    }

    /// Look up a tool by name.
    fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Get the skill index that owns the given tool.
    fn skill_index(&self, tool_name: &str) -> Option<usize> {
        self.tool_to_skill.get(tool_name).copied()
    }

    /// Return tool definitions for the LLM, or `&[]` when disabled.
    fn definitions_for_llm(&self) -> &[ToolDefinition] {
        if self.disabled {
            &[]
        } else {
            &self.definitions
        }
    }

    /// Mark tools as disabled — subsequent LLM calls will omit definitions.
    fn disable(&mut self) {
        self.disabled = true;
    }
}

/// Mutable conversation state — the session-scoped data that changes during
/// `run()` calls.
pub(crate) struct Conversation {
    /// Conversation history.  Persists across `run()` calls.
    pub(crate) messages: Vec<Message>,

    /// Number of user turns processed (for dream trigger timing).
    pub(crate) turn_count: usize,

    /// Token usage tracking and optional budget enforcement.
    pub token_budget: TokenBudget,
}

impl Conversation {
    fn new() -> Self {
        Self {
            messages: Vec::new(),
            turn_count: 0,
            token_budget: TokenBudget::default(),
        }
    }
}

/// Persistence backend for rotating pre-compaction conversation snapshots.
///
/// When attached to the agent, compaction saves the full verbatim
/// conversation to a timestamped archive before summarising, preserving
/// history for fine-tuning datasets.
pub(crate) struct HistoryBackend {
    pub(crate) store: Arc<dyn ChatHistory>,
    pub(crate) chat_id: String,
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// The streaming tool-use agent — Dyson's core runtime.
///
/// Created once at startup, then `run()` is called for each user message.
/// Conversation history (`conversation.messages`) persists across calls for
/// multi-turn conversations.
pub struct Agent {
    /// LLM client for streaming completions, gated by rate limiting.
    ///
    /// Stored as a [`RateLimitedHandle`] so that multiple agents can share
    /// the same underlying LLM client and rate-limit window.  The handle
    /// is created by the controller from a single [`RateLimited`] source.
    client: rate_limiter::RateLimitedHandle<Box<dyn LlmClient>>,

    /// Sandbox that gates all tool execution.
    ///
    /// Wrapped in `Arc` (not `Box`) so subagents can share the parent's
    /// sandbox without cloning.  This ensures child agents inherit the
    /// same security policy — subagents cannot bypass the sandbox.
    sandbox: Arc<dyn Sandbox>,

    /// Loaded skills (retained for lifecycle: before_turn, after_tool, on_unload).
    skills: Vec<Box<dyn Skill>>,

    /// Immutable tool registry — tool lookup, definitions, and token cache.
    tool_registry: ToolRegistry,

    /// Composed system prompt: base prompt + all skill prompt fragments.
    system_prompt: Arc<str>,

    /// LLM configuration (model, max_tokens, temperature).
    config: CompletionConfig,

    /// Maximum LLM turns per `run()` call.
    max_iterations: usize,

    /// Maximum retries on transient LLM errors (HTTP 429, 529, network).
    max_retries: usize,

    /// Mutable conversation state (messages, turn count, token budget).
    pub(crate) conversation: Conversation,

    /// Shared tool context (working dir, env, cancellation).
    tool_context: ToolContext,

    /// Context compaction configuration.
    /// When set, the agent automatically compacts conversation history when
    /// the estimated context size exceeds `compaction_config.threshold()`.
    compaction_config: Option<CompactionConfig>,

    /// Per-turn tool call rate limiter.
    limiter: ToolLimiter,

    /// Structured result formatter for LLM-optimized tool output.
    formatter: ResultFormatter,

    /// Pre/post tool execution hooks.
    ///
    /// Hooks can block, modify, or observe tool calls before and after
    /// execution.  See `tool_hooks.rs`.
    tool_hooks: Vec<Box<dyn crate::tool_hooks::ToolHook>>,

    /// Handle to the persistent dream thread — fires background cognitive
    /// tasks (memory maintenance, learning synthesis, self-improvement)
    /// on trigger events without blocking the controller loop.
    /// See `dream.rs` and `docs/dreaming.md`.
    dream_handle: DreamHandle,

    /// Optional persistence backend for rotating pre-compaction snapshots.
    history_backend: Option<HistoryBackend>,

    /// Optional audio transcriber for resolving media attachments.
    ///
    /// Created from `Settings::transcriber` config at agent construction time.
    /// When present, the agent can resolve audio attachments in
    /// `run_with_attachments()`.  Images and PDFs do not require a transcriber.
    transcriber: Option<std::sync::Arc<dyn crate::media::audio::Transcriber>>,
}

// ---------------------------------------------------------------------------
// AgentBuilder
// ---------------------------------------------------------------------------

/// Fluent builder for `Agent` — makes construction intent explicit.
///
/// Every agent construction path (full agent, provider switch, group chat)
/// goes through this builder so the call site reads clearly:
///
/// ```rust,ignore
/// Agent::builder(client, sandbox)
///     .skills(skills)
///     .settings(&settings)
///     .workspace(ws)
///     .nudge_interval(5)
///     .build()
/// ```
pub struct AgentBuilder {
    client: rate_limiter::RateLimitedHandle<Box<dyn LlmClient>>,
    sandbox: Arc<dyn Sandbox>,
    skills: Vec<Box<dyn Skill>>,
    settings: AgentSettings,
    workspace: Option<std::sync::Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>>>,
    nudge_interval: usize,
    transcriber: Option<std::sync::Arc<dyn crate::media::audio::Transcriber>>,
}

impl AgentBuilder {
    /// Set the skills (and their tools) available to the agent.
    pub fn skills(mut self, skills: Vec<Box<dyn Skill>>) -> Self {
        self.skills = skills;
        self
    }

    /// Set agent settings (model, system prompt, max_tokens, etc.).
    pub fn settings(mut self, settings: &AgentSettings) -> Self {
        self.settings = settings.clone();
        self
    }

    /// Attach a workspace for identity, memory, and working directory.
    pub fn workspace(
        mut self,
        ws: std::sync::Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>>,
    ) -> Self {
        self.workspace = Some(ws);
        self
    }

    /// Set the dream nudge interval (0 = no dreams).
    pub fn nudge_interval(mut self, n: usize) -> Self {
        self.nudge_interval = n;
        self
    }

    /// Attach a transcriber for resolving audio attachments.
    pub fn transcriber(mut self, t: std::sync::Arc<dyn crate::media::audio::Transcriber>) -> Self {
        self.transcriber = Some(t);
        self
    }

    /// Build the agent. Consumes the builder.
    pub fn build(self) -> Result<Agent> {
        Agent::new(
            self.client,
            self.sandbox,
            self.skills,
            &self.settings,
            self.workspace,
            self.nudge_interval,
            self.transcriber,
        )
    }
}

impl Agent {
    /// Start building an agent with the two required components.
    ///
    /// The `client` handle comes from a shared [`RateLimited`] owned by
    /// the controller — all agents built from the same source share one
    /// LLM client and one rate-limit window.
    pub fn builder(
        client: rate_limiter::RateLimitedHandle<Box<dyn LlmClient>>,
        sandbox: Arc<dyn Sandbox>,
    ) -> AgentBuilder {
        AgentBuilder {
            client,
            sandbox,
            skills: Vec::new(),
            settings: AgentSettings::default(),
            workspace: None,
            nudge_interval: 0,
            transcriber: None,
        }
    }

    /// Construct a new agent from its components.
    ///
    /// Prefer [`Agent::builder()`] for new code — it makes intent explicit.
    ///
    /// Delegates to focused constructors:
    /// - [`ToolRegistry::from_skills`] — flattens skills' tools into a lookup map.
    /// - [`Self::compose_system_prompt`] — assembles base + model info + skill fragments.
    /// - [`Self::build_dream_handle`] — configures the background dream thread.
    /// - [`Self::build_tool_context`] — resolves working directory from workspace.
    ///
    /// ## Panics
    ///
    /// Does not panic.  Duplicate tool names are handled by last-writer-wins
    /// (later skills override earlier ones), with a warning logged.
    pub fn new(
        client: rate_limiter::RateLimitedHandle<Box<dyn LlmClient>>,
        sandbox: Arc<dyn Sandbox>,
        skills: Vec<Box<dyn Skill>>,
        settings: &AgentSettings,
        workspace: Option<
            std::sync::Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>>,
        >,
        nudge_interval: usize,
        transcriber: Option<std::sync::Arc<dyn crate::media::audio::Transcriber>>,
    ) -> Result<Self> {
        let tool_registry = ToolRegistry::from_skills(&skills);
        let system_prompt = Self::compose_system_prompt(settings, &skills);
        let tool_context = Self::build_tool_context(&sandbox, workspace);
        let dream_handle = Self::build_dream_handle(&tool_context, nudge_interval);

        let config = CompletionConfig {
            model: settings.model.clone(),
            max_tokens: settings.max_tokens,
            temperature: None, // use provider default
        };

        // Expose tools via MCP for CLI backends (no-op for API clients).
        client.get_ref().set_mcp_tools(tool_registry.tools.clone());

        tracing::info!(
            skill_count = skills.len(),
            tool_count = tool_registry.definitions.len(),
            "agent initialized"
        );

        Ok(Self {
            client,
            sandbox,
            skills,
            tool_registry,
            system_prompt: Arc::from(system_prompt),
            config,
            max_iterations: settings.max_iterations,
            max_retries: 3,
            conversation: Conversation::new(),
            tool_context,
            compaction_config: settings.compaction,
            limiter: ToolLimiter::for_agent(),
            formatter: ResultFormatter::default(),
            tool_hooks: Vec::new(),
            dream_handle,
            history_backend: None,
            transcriber,
        })
    }

    /// Compose the system prompt from base + model info + skill fragments.
    fn compose_system_prompt(settings: &AgentSettings, skills: &[Box<dyn Skill>]) -> String {
        let mut system_prompt = settings.system_prompt.clone();

        // Inject model/provider info so the model can answer "what model
        // are you running?" accurately.
        system_prompt.push_str(&format!(
            "\n\nYou are running on model '{}' via the {:?} provider.",
            settings.model, settings.provider,
        ));

        for skill in skills {
            if let Some(fragment) = skill.system_prompt() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(fragment);
            }
        }

        system_prompt
    }

    /// Resolve the working directory from the workspace and build a ToolContext.
    fn build_tool_context(
        sandbox: &Arc<dyn Sandbox>,
        workspace: Option<
            std::sync::Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>>,
        >,
    ) -> ToolContext {
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
            dangerous_no_sandbox: sandbox.skip_path_validation(),
        };
        tool_context.workspace = workspace;
        tool_context
    }

    /// Build the persistent dream thread from workspace configuration.
    fn build_dream_handle(tool_context: &ToolContext, nudge_interval: usize) -> DreamHandle {
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

        let dream_count = dreams.len();
        let dream_handle = DreamHandle::new(dreams);
        tracing::info!(dream_count, nudge_interval, "dream subsystem initialised");
        dream_handle
    }

    /// Get a shared reference to the sandbox for subagent reuse.
    ///
    /// Subagents share the parent's sandbox to ensure consistent security
    /// policy across the agent hierarchy.
    pub fn sandbox(&self) -> &Arc<dyn Sandbox> {
        &self.sandbox
    }

    /// Register tool hooks for pre/post tool execution lifecycle events.
    pub fn set_tool_hooks(&mut self, hooks: Vec<Box<dyn crate::tool_hooks::ToolHook>>) {
        self.tool_hooks = hooks;
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
    /// Snapshots messages into an `Arc<[Message]>` so the dream thread owns
    /// a shared reference — avoids cloning the entire conversation Vec on
    /// every turn.  The snapshot is only materialised when dreams are
    /// actually going to fire (workspace present, messages non-empty).
    fn fire_dreams(&self, event: DreamEvent) {
        if self.conversation.messages.is_empty() {
            tracing::debug!(?event, "fire_dreams skipped: no messages");
            return;
        }
        if self.tool_context.workspace.is_none() {
            tracing::debug!(?event, "fire_dreams skipped: no workspace");
            return;
        }

        tracing::debug!(
            ?event,
            turn_count = self.conversation.turn_count,
            "sending dream event"
        );

        // Snapshot into Arc<[Message]> — the dream thread converts to Vec
        // only if a dream actually activates and needs to summarise.
        let messages: Arc<[Message]> = self.conversation.messages.clone().into();

        self.dream_handle.fire(
            event,
            self.client.with_priority(rate_limiter::Priority::Background),
            self.config.clone(),
            self.tool_context.clone(),
            messages,
            self.conversation.turn_count,
        );
    }

    /// Clear conversation history, firing session-end dreams in the background.
    ///
    /// Messages are cleared immediately so the caller can continue.  Dreams
    /// run in the background with no way to block the caller.
    pub fn clear(&mut self) {
        self.fire_dreams(DreamEvent::SessionEnd);
        self.conversation.messages.clear();
    }

    /// Get the current conversation messages (for persistence).
    pub fn messages(&self) -> &[Message] {
        &self.conversation.messages
    }

    /// Replace the conversation history (for restoring from persistence).
    pub fn set_messages(&mut self, messages: Vec<Message>) {
        self.conversation.messages = messages;
    }

    /// Attach a chat history backend so compaction can rotate pre-compaction
    /// snapshots for fine-tuning.
    ///
    /// When set, every compaction will first save the current conversation
    /// to a timestamped archive file (via `ChatHistory::rotate`) before
    /// summarising.  This preserves the full verbatim history.
    pub fn set_chat_history(&mut self, store: Arc<dyn ChatHistory>, chat_id: String) {
        self.history_backend = Some(HistoryBackend { store, chat_id });
    }

    /// Remove and return the last message in the conversation history.
    fn pop_last_message(&mut self) -> Option<Message> {
        self.conversation.messages.pop()
    }

    /// Replace all `ContentBlock::Image` blocks in the conversation history
    /// with `[image]` placeholder text.  Called when the active model does
    /// not support vision — sanitises the entire history so subsequent
    /// turns don't replay rejected image data.
    fn strip_images(&mut self) {
        for msg in &mut self.conversation.messages {
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
    fn disable_tools(&mut self) {
        self.tool_registry.disable();
    }

    /// Replace all `ContentBlock::ToolUse` and `ContentBlock::ToolResult`
    /// blocks in the conversation history with text placeholders.
    ///
    /// Called when the active model doesn't support tool use — the OpenAI
    /// serializer would otherwise emit `role: "tool"` messages and
    /// `tool_calls` arrays that providers reject when no tool definitions
    /// are provided.
    fn strip_tool_history(&mut self) {
        for msg in &mut self.conversation.messages {
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

    /// Hot-swap the LLM client and model without rebuilding the agent.
    ///
    /// Updates the client handle, model name in `CompletionConfig`, and the
    /// provider/model line in the system prompt.  Everything else (skills,
    /// tools, sandbox, conversation, workspace) is preserved.
    ///
    /// This is the fast path for `/model` switches — no allocations beyond
    /// the new handle and updated strings.
    pub fn swap_client(
        &mut self,
        new_client: rate_limiter::RateLimitedHandle<Box<dyn LlmClient>>,
        model: &str,
        provider: &crate::config::LlmProvider,
    ) {
        self.client = new_client;
        self.config.model = model.to_string();
        self.client
            .get_ref()
            .set_mcp_tools(self.tool_registry.tools.clone());

        // Patch the "You are running on model …" line in the system prompt.
        let marker = "\n\nYou are running on model '";
        if let Some(pos) = self.system_prompt.find(marker) {
            let end = self.system_prompt[pos..]
                .find('.')
                .map(|i| pos + i + 1)
                .unwrap_or(self.system_prompt.len());
            let mut new_prompt = String::with_capacity(self.system_prompt.len());
            new_prompt.push_str(&self.system_prompt[..pos]);
            new_prompt.push_str(&format!(
                "\n\nYou are running on model '{model}' via the {provider:?} provider.",
            ));
            new_prompt.push_str(&self.system_prompt[end..]);
            self.system_prompt = Arc::from(new_prompt);
        }

        tracing::info!(model, provider = ?provider, "client swapped");
    }

    /// Get the system prompt (for quick response context).
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Check whether a tool is registered by name.
    #[cfg(test)]
    pub fn has_tool(&self, name: &str) -> bool {
        self.tool_registry.tools.contains_key(name)
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
    /// Provide direct access to the token budget for external inspection.
    pub fn token_budget(&self) -> &TokenBudget {
        &self.conversation.token_budget
    }

    /// Provide mutable access to the token budget for external configuration.
    pub fn token_budget_mut(&mut self) -> &mut TokenBudget {
        &mut self.conversation.token_budget
    }

    pub async fn run(&mut self, user_input: &str, output: &mut dyn Output) -> Result<String> {
        tracing::info!(
            input_len = user_input.len(),
            input_preview = &user_input[..user_input.len().min(200)],
            "user message received"
        );
        // Append the user's message to history.
        self.conversation.messages.push(Message::user(user_input));
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
        self.conversation.messages.push(Message::user_multimodal(blocks));
        self.run_inner(output).await
    }

    /// Run the agent loop with text and raw media attachments.
    ///
    /// Resolves each attachment into ContentBlocks (images are resized,
    /// audio is transcribed, PDFs are extracted), then builds a multimodal
    /// user message and enters the agent loop.
    ///
    /// This is the primary entry point for controllers with media support.
    /// Controllers only need to provide raw bytes + MIME type — the agent
    /// handles all resolution.
    pub async fn run_with_attachments(
        &mut self,
        text: &str,
        attachments: Vec<crate::media::Attachment>,
        output: &mut dyn Output,
    ) -> Result<String> {
        tracing::info!(
            text_len = text.len(),
            attachment_count = attachments.len(),
            "user message with attachments received"
        );

        let mut blocks = Vec::new();
        if !text.is_empty() {
            blocks.push(ContentBlock::Text {
                text: text.to_string(),
            });
        }

        for attachment in attachments {
            let mime = attachment.mime_type.clone();
            match crate::media::resolve_attachment(attachment, self.transcriber.as_ref()).await {
                Ok(resolved) => blocks.extend(resolved),
                Err(e) => {
                    tracing::warn!(error = %e, mime_type = %mime, "failed to resolve attachment");
                    blocks.push(ContentBlock::Text {
                        text: format!("[Failed to process {mime} attachment: {e}]"),
                    });
                }
            }
        }

        self.conversation
            .messages
            .push(Message::user_multimodal(blocks));
        self.run_inner(output).await
    }

    /// Inner agent loop shared by [`run()`], [`run_with_blocks()`], and
    /// [`run_with_attachments()`].
    ///
    /// Assumes the caller has already pushed the user message to
    /// `self.conversation.messages`.
    async fn run_inner(&mut self, output: &mut dyn Output) -> Result<String> {
        self.conversation.turn_count += 1;

        self.fire_dreams(DreamEvent::TurnComplete {
            turn_count: self.conversation.turn_count,
        });

        let mut final_text = String::new();
        let mut hit_max_iterations = false;

        let skill_fragments = self.collect_skill_context().await;

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
            self.auto_compact_if_needed(&turn_system_prompt, output).await;
            self.log_iteration(iteration);

            output.typing_indicator(true)?;

            // Stream LLM response with retry/backoff.
            let response = match self
                .stream_with_retry(&skill_fragments, &mut recovered_this_turn, output)
                .await
            {
                StreamResult::Response(r) => r,
                StreamResult::Recovered => continue,
                StreamResult::Error(e) => return Err(e),
            };

            let tool_mode = response.tool_mode;
            if let Some(input_tokens) = response.input_tokens {
                self.conversation.token_budget.record_input(input_tokens);
            }

            tracing::info!(
                tool_mode = ?tool_mode,
                input_tokens = ?response.input_tokens,
                "streaming response"
            );

            let (assistant_msg, tool_calls, output_tokens) =
                stream_handler::process_stream(response.stream, output).await?;

            if let Err(e) = self.conversation.token_budget.record(output_tokens) {
                self.conversation.messages.push(assistant_msg);
                tracing::warn!(
                    used = self.conversation.token_budget.output_tokens_used,
                    "token budget exceeded — stopping agent loop"
                );
                output.error(&e)?;
                break;
            }

            self.log_response(&assistant_msg, &tool_calls);

            // If no tool calls or provider handles tools internally, we're done.
            if tool_calls.is_empty() || tool_mode == crate::llm::ToolMode::Observe {
                if let Some(text) = assistant_msg.last_text() {
                    final_text = text.to_string();
                }
                self.conversation.messages.push(assistant_msg);
                output.flush()?;
                break;
            }

            self.conversation.messages.push(assistant_msg);
            self.execute_tool_calls(&tool_calls, output).await?;
            self.limiter.reset_turn();

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
        }

        output.flush()?;
        Ok(final_text)
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
        fragments
    }

    /// Auto-compact if estimated context tokens exceed the threshold.
    ///
    /// When no explicit `CompactionConfig` is set, falls back to defaults
    /// (200k context window, 50% threshold) so conversations always have a
    /// safety net against unbounded growth.
    async fn auto_compact_if_needed(
        &mut self,
        turn_system_prompt: &str,
        output: &mut dyn Output,
    ) {
        let config = self.compaction_config.unwrap_or_default();
        if self.conversation.messages.len() <= config.protect_head {
            return;
        }
        let threshold = config.threshold();
        let estimated_tokens = self.estimate_context_tokens(turn_system_prompt);
        if estimated_tokens > threshold {
            tracing::info!(
                estimated_tokens,
                threshold,
                messages = self.conversation.messages.len(),
                has_explicit_config = self.compaction_config.is_some(),
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
                    crate::message::ContentBlock::Document { .. } => "document".to_string(),
                    crate::message::ContentBlock::Thinking { .. } => "thinking".to_string(),
                }).collect();
                tracing::debug!(
                    msg_index = i,
                    role,
                    blocks = ?block_summary,
                    "message in context"
                );
            }
        }
    }

    /// Stream an LLM response with retry/backoff and error recovery.
    async fn stream_with_retry(
        &mut self,
        skill_fragments: &str,
        recovered_this_turn: &mut bool,
        output: &mut dyn Output,
    ) -> StreamResult {
        let mut last_err = None;
        let mut response_opt = None;
        let mut recovery: Option<LlmRecovery> = None;

        for attempt in 0..=self.max_retries {
            let tools_for_llm = self.tool_registry.definitions_for_llm();

            match self
                .client
                .access()
                .map_err(StreamResult::Error)
            {
                Ok(client) => {
                    match client
                        .stream(
                            &self.conversation.messages,
                            &self.system_prompt,
                            skill_fragments,
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
                                delay_ms,
                                error = %e,
                                "LLM call failed, retrying"
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                            last_err = Some(e);
                        }
                        Err(e) => {
                            if *recovered_this_turn {
                                return StreamResult::Error(e);
                            }
                            let action = output.on_llm_error(&e);
                            if action == LlmRecovery::GiveUp {
                                return StreamResult::Error(e);
                            }
                            recovery = Some(action);
                            break;
                        }
                    }
                }
                Err(result) => return result,
            }
        }

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
                self.conversation.messages.push(msg);
            }
            *recovered_this_turn = true;
            return StreamResult::Recovered;
        }

        match response_opt {
            Some(r) => StreamResult::Response(r),
            None => StreamResult::Error(last_err.unwrap_or_else(|| {
                DysonError::Llm("retries exhausted with no error captured".into())
            })),
        }
    }

    /// Log a summary of the assistant response.
    fn log_response(&self, assistant_msg: &Message, tool_calls: &[ToolCall]) {
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

// Tests are in agent/tests.rs
