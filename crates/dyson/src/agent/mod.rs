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

mod budget;
mod compaction;
mod dependency_analyzer;
pub mod dream;
mod execution;
pub mod rate_limiter;
mod reflection;
mod result_formatter;
mod retry;
mod silent_output;
pub use silent_output::SilentOutput;
mod r#loop;
mod persistence;
mod sliding_window;
mod state;
pub mod stream_handler;
pub mod token_budget;
mod tool_limiter;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use self::dream::{DreamEvent, DreamHandle};
use self::result_formatter::ResultFormatter;
#[cfg(test)]
use self::stream_handler::ToolCall;
use self::tool_limiter::ToolLimiter;
use crate::config::{AgentSettings, CompactionConfig};
use crate::controller::Output;
#[cfg(test)]
use crate::error::DysonError;
use crate::error::Result;
#[cfg(test)]
use crate::llm::ToolDefinition;
use crate::llm::{CompletionConfig, LlmClient};
use crate::message::{ContentBlock, Message};
use crate::sandbox::Sandbox;
use crate::skill::Skill;
use crate::tool::{Tool, ToolContext};

use self::state::{Conversation, HistoryBackend, ToolRegistry, restored_turn_count};
use self::token_budget::TokenBudget;

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

    /// Maximum retries on transient LLM failures: HTTP 429/529, network
    /// errors, and empty responses (no text, no tool calls).
    max_retries: usize,

    /// Mutable conversation state (messages, turn count, token budget).
    pub(crate) conversation: Conversation,

    /// Shared tool context (working dir, env, cancellation).
    tool_context: ToolContext,

    /// Context compaction configuration.
    /// The agent automatically compacts conversation history when the
    /// estimated context size exceeds `compaction_config.threshold()`.
    compaction_config: CompactionConfig,

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

    /// Optional feedback store for per-turn quality ratings.
    ///
    /// When set, dreams incorporate user ratings into their decisions:
    /// memory curation prioritises insights from well-rated interactions,
    /// and self-improvement uses ratings to guide skill creation.
    feedback_store: Option<crate::feedback::FeedbackStore>,

    /// Optional audio transcriber for resolving media attachments.
    ///
    /// Created from `Settings::transcriber` config at agent construction time.
    /// When present, the agent can resolve audio attachments in
    /// `run_with_attachments()`.  Images and PDFs do not require a transcriber.
    transcriber: Option<std::sync::Arc<dyn crate::media::audio::Transcriber>>,

    /// Ephemeral prompt fragment when an advisor is active.
    /// Appended to skill_fragments (dynamic area) to avoid busting KV cache.
    advisor_prompt: Option<&'static str>,

    /// Optional hook fired whenever the agent's message history changes.
    /// Controllers use it to checkpoint the transcript to disk during a
    /// long turn (e.g. a subagent that streams for minutes) so a crash
    /// or kill doesn't lose the conversation — the end-of-turn save is
    /// unreachable if the tokio task gets aborted mid-run.
    persist_hook: Option<PersistHook>,
}

/// Callback fired whenever the agent's message history changes.
/// See [`Agent::set_persist_hook`] for usage.
pub type PersistHook = std::sync::Arc<dyn Fn(&[crate::message::Message]) + Send + Sync>;

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
    workspace: Option<crate::workspace::WorkspaceHandle>,
    nudge_interval: usize,
    transcriber: Option<std::sync::Arc<dyn crate::media::audio::Transcriber>>,
    advisor: Option<Box<dyn crate::advisor::Advisor>>,
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
    pub fn workspace(mut self, ws: crate::workspace::WorkspaceHandle) -> Self {
        self.workspace = Some(ws);
        self
    }

    /// Set the dream nudge interval (0 = no dreams).
    pub const fn nudge_interval(mut self, n: usize) -> Self {
        self.nudge_interval = n;
        self
    }

    /// Attach a transcriber for resolving audio attachments.
    pub fn transcriber(mut self, t: std::sync::Arc<dyn crate::media::audio::Transcriber>) -> Self {
        self.transcriber = Some(t);
        self
    }

    /// Attach an advisor — a stronger model the executor can consult.
    pub fn advisor(mut self, advisor: Box<dyn crate::advisor::Advisor>) -> Self {
        self.advisor = Some(advisor);
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
            self.advisor,
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
            advisor: None,
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
    // Eight parameters is intentional for this constructor: each is a distinct
    // collaborator (client, sandbox, skills, settings, workspace, ...) and
    // grouping them into a struct would just reshuffle the cost to callers.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: rate_limiter::RateLimitedHandle<Box<dyn LlmClient>>,
        sandbox: Arc<dyn Sandbox>,
        skills: Vec<Box<dyn Skill>>,
        settings: &AgentSettings,
        workspace: Option<crate::workspace::WorkspaceHandle>,
        nudge_interval: usize,
        transcriber: Option<std::sync::Arc<dyn crate::media::audio::Transcriber>>,
        advisor: Option<Box<dyn crate::advisor::Advisor>>,
    ) -> Result<Self> {
        let mut tool_registry = ToolRegistry::from_skills(&skills);

        // Bind the advisor to the parent's resources, then register its tools
        // and collect API injections.
        let mut advisor = advisor;
        let api_tool_injections = if let Some(ref mut advisor) = advisor {
            // Collect the parent's tools for the advisor to inherit.
            let inherited_tools: Vec<Arc<dyn Tool>> =
                tool_registry.tools.values().cloned().collect();
            advisor.bind(
                Arc::clone(&sandbox),
                workspace.as_ref().map(Arc::clone),
                inherited_tools,
            );
            for tool in advisor.tools() {
                tool_registry.register_extra_tool(tool);
            }
            advisor.api_tool_entries()
        } else {
            vec![]
        };

        let advisor_prompt = if advisor.is_some() {
            Some(
                "\n\nYou have access to an `advisor` tool — a more capable model \
                  you can consult for complex decisions. Use it when facing \
                  architectural choices, ambiguous trade-offs, or problems that \
                  would benefit from a second opinion. The advisor can read files \
                  and investigate the codebase. Don't use it for simple tasks.",
            )
        } else {
            None
        };

        let system_prompt = Self::compose_system_prompt(settings, &skills);
        let tool_context = Self::build_tool_context(&sandbox, workspace);
        let dream_handle = Self::build_dream_handle(&tool_context, nudge_interval);

        let config = CompletionConfig {
            model: settings.model.clone(),
            max_tokens: settings.max_tokens,
            temperature: None, // use provider default
            api_tool_injections,
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
            max_retries: settings.max_retries,
            conversation: Conversation::new(),
            tool_context,
            compaction_config: settings.compaction,
            limiter: ToolLimiter::for_agent(),
            formatter: ResultFormatter::default(),
            tool_hooks: Vec::new(),
            dream_handle,
            history_backend: None,
            feedback_store: None,
            transcriber,
            advisor_prompt,
            persist_hook: None,
        })
    }

    /// Compose the system prompt from base + model info + skill fragments.
    fn compose_system_prompt(settings: &AgentSettings, skills: &[Box<dyn Skill>]) -> String {
        let mut system_prompt = settings.system_prompt.clone();

        // Inject model/provider info so the model can answer "what model
        // are you running?" accurately.
        write!(
            &mut system_prompt,
            "\n\nYou are running on model '{}' via the {:?} provider.",
            settings.model, settings.provider,
        )
        .unwrap();

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
        workspace: Option<crate::workspace::WorkspaceHandle>,
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
            // Inherit the bypass from the parent sandbox, if any.  This is
            // the only out-of-CLI mint: a subagent runs against the parent
            // sandbox's posture, which is already validated at startup.
            sandbox_bypass: sandbox
                .sandbox_bypass()
                .map(|_| crate::sandbox::SandboxBypassGuard::inherited_from_parent()),
            taint_indexes: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            activity: None,
            tool_use_id: None,
            subagent_events: None,
            artefacts: None,
            current_chat_id: None,
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
                dreams.push(Arc::new(reflection::MemoryMaintenanceDream::new(
                    nudge_interval,
                )));

                // Self-improvement: create skills every 2N turns.
                dreams.push(Arc::new(reflection::SelfImprovementDream::new(
                    nudge_interval,
                )));
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
    pub const fn set_depth(&mut self, depth: u8) {
        self.tool_context.depth = depth;
    }

    /// Override the working directory on this agent's tool context.
    ///
    /// Called by `CoderTool` after construction to scope the child
    /// agent to a specific subdirectory.
    pub fn set_working_dir(&mut self, dir: std::path::PathBuf) {
        self.tool_context.working_dir = dir;
    }

    /// Replace the agent's tool-context cancellation token.
    ///
    /// Lets a controller install a fresh per-task `CancellationToken`
    /// so an external cancel signal can cooperatively abort any tool
    /// calls that observe the token (currently `web_fetch` and
    /// `web_search`; other tools will drop naturally when the agent
    /// future is dropped).
    pub fn set_cancellation_token(&mut self, token: CancellationToken) {
        self.tool_context.cancellation = token;
    }

    /// Install a chat-scoped activity handle on this agent's tool
    /// context.  Populated by the HTTP controller per chat; other
    /// controllers leave it unset.  UI-only side channel — see
    /// `ToolContext::activity` for the LLM-boundary note.
    pub fn set_activity_handle(&mut self, handle: crate::controller::ActivityHandle) {
        self.tool_context.activity = Some(handle);
    }

    /// Install a per-turn subagent UI events bus.  Wired only by the
    /// HTTP controller — lets nested tool calls inside a subagent surface
    /// in the web UI without flowing into the parent's LLM conversation.
    /// See `ToolContext::subagent_events` for the boundary invariant.
    pub fn set_subagent_events(&mut self, bus: crate::controller::http::SubagentEventBus) {
        self.tool_context.subagent_events = Some(bus);
    }

    /// Bind instance-wide artefact access for controller-scoped runs.
    ///
    /// Artefact bodies are intentionally not serialized back into model history.
    /// Controllers that own an artefact store can install this reader so the
    /// `artifacts` tool can list/read documents across the whole instance.
    /// `chat_id` is still stored as ambient context for scripts and for explicit
    /// `artifacts` calls that filter/disambiguate by chat.
    pub fn set_artefact_reader(
        &mut self,
        reader: Arc<dyn crate::tool::artefacts::ArtefactReader>,
        chat_id: impl Into<String>,
    ) {
        self.tool_context.artefacts = Some(reader);
        self.tool_context.current_chat_id = Some(chat_id.into());
    }

    /// Send a dream event to the persistent dream thread.
    ///
    /// Pre-checks triggers on the main thread so the expensive message
    /// snapshot (`Arc<[Message]>`) is only materialised when at least one
    /// dream will activate.  This avoids cloning the entire conversation
    /// Vec on turns where no dream fires (e.g. turn 3 with EveryNTurns(5)).
    fn fire_dreams(&self, event: DreamEvent) {
        if self.conversation.messages.is_empty() {
            tracing::debug!(?event, "fire_dreams skipped: no messages");
            return;
        }
        if self.tool_context.workspace.is_none() {
            tracing::debug!(?event, "fire_dreams skipped: no workspace");
            return;
        }
        if !self.dream_handle.would_fire(&event) {
            tracing::debug!(?event, "fire_dreams skipped: no dream matches");
            return;
        }

        tracing::debug!(
            ?event,
            turn_count = self.conversation.turn_count,
            "sending dream event"
        );

        let messages: Arc<[Message]> = self.conversation.messages.clone().into();

        let feedback_entries = self
            .feedback_store
            .as_ref()
            .zip(self.history_backend.as_ref())
            .and_then(|(store, backend)| match store.load(&backend.chat_id) {
                Ok(entries) if !entries.is_empty() => Some(entries),
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to load feedback for dreams");
                    None
                }
            });

        self.dream_handle.fire(dream::DreamRequest {
            event,
            client: self
                .client
                .with_priority(rate_limiter::Priority::Background),
            config: self.config.clone(),
            tool_context: self.tool_context.clone(),
            messages,
            turn_count: self.conversation.turn_count,
            feedback_entries,
        });
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
        self.conversation.turn_count = restored_turn_count(&messages);
        self.conversation.messages = messages;
    }

    /// Append a direct controller-handled turn, such as an executable
    /// local-skill slash command that bypassed the LLM.
    pub fn append_direct_turn(&mut self, user_input: &str, assistant_output: &str) {
        self.conversation.messages.push(Message::user(user_input));
        self.conversation
            .messages
            .push(Message::assistant(vec![ContentBlock::Text {
                text: assistant_output.to_string(),
            }]));
        self.conversation.turn_count += 1;
        self.persist();
    }

    /// Attach a feedback store so dreams can incorporate user ratings.
    pub fn set_feedback_store(&mut self, store: crate::feedback::FeedbackStore) {
        self.feedback_store = Some(store);
    }

    /// Record a user's quality rating for a specific assistant turn.
    pub fn record_feedback(
        &self,
        turn_index: usize,
        rating: crate::feedback::FeedbackRating,
    ) -> crate::error::Result<()> {
        let (Some(store), Some(backend)) = (&self.feedback_store, &self.history_backend) else {
            return Ok(());
        };
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        store.upsert(
            &backend.chat_id,
            crate::feedback::FeedbackEntry {
                turn_index,
                rating,
                score: rating.score(),
                timestamp,
            },
        )
    }

    /// Remove a previously recorded rating for a specific turn.
    pub fn remove_feedback(&self, turn_index: usize) -> crate::error::Result<()> {
        let (Some(store), Some(backend)) = (&self.feedback_store, &self.history_backend) else {
            return Ok(());
        };
        store.remove(&backend.chat_id, turn_index)
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
    const fn disable_tools(&mut self) {
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
            write!(
                &mut new_prompt,
                "\n\nYou are running on model '{model}' via the {provider:?} provider.",
            )
            .unwrap();
            new_prompt.push_str(&self.system_prompt[end..]);
            self.system_prompt = Arc::from(new_prompt);
        }

        tracing::info!(model, provider = ?provider, "client swapped");
    }

    /// Get the system prompt (for quick response context).
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Set user attribution on the workspace for write auditing.
    ///
    /// Call before each `run()` in public-agent contexts to record which
    /// user triggered any memory writes.  Pass `None` to clear.
    pub async fn set_attribution(&self, user: Option<&str>) {
        if let Some(ws) = &self.tool_context.workspace {
            ws.write().await.set_attribution(user);
        }
    }

    /// Check whether a tool is registered by name.
    pub fn has_tool(&self, name: &str) -> bool {
        self.tool_registry.tools.contains_key(name)
    }

    /// Register an extra tool not owned by any skill.
    ///
    /// Lets a controller expose a controller-local tool to its agent
    /// without dragging the schema into deploys that don't need it.
    pub fn register_tool(&mut self, tool: Arc<dyn Tool>) {
        self.tool_registry.register_extra_tool(tool);
    }

    /// Get the names of all registered tools.
    ///
    /// Returns tool names sorted for deterministic output.
    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tool_registry.tools.keys().cloned().collect();
        names.sort();
        names
    }

    /// Get the completion config (for quick response context).
    pub const fn config(&self) -> &CompletionConfig {
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
    pub const fn token_budget(&self) -> &TokenBudget {
        &self.conversation.token_budget
    }

    /// Provide mutable access to the token budget for external configuration.
    pub const fn token_budget_mut(&mut self) -> &mut TokenBudget {
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
        self.persist();
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
        self.conversation
            .messages
            .push(Message::user_multimodal(blocks));
        self.persist();
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
        self.persist();
        self.run_inner(output).await
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
        api_tool_injections: vec![],
    };

    // Augment the system prompt to signal brevity.
    let quick_suffix = "\n\nIMPORTANT: You are answering a quick question while \
        your main processing is busy on a previous request. Be concise and direct. \
        You have no tools available right now.";

    let response = client
        .stream(&msgs, system_prompt, quick_suffix, &[], &quick_config)
        .await?;

    // Process the stream — reuse the standard handler.
    let (assistant_msg, _tool_calls, _output_tokens, _stop_reason) =
        stream_handler::process_stream(response.stream, output).await?;

    output.flush()?;

    // Extract the final text.
    let final_text = assistant_msg.last_text().unwrap_or_default().to_string();

    Ok(final_text)
}

// Tests are in agent/tests.rs
