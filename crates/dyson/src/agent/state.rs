use std::collections::HashMap;
use std::sync::Arc;

use crate::chat_history::ChatHistory;
use crate::llm::ToolDefinition;
use crate::message::{ContentBlock, Message, Role};
use crate::skill::Skill;
use crate::tool::Tool;

use super::token_budget::TokenBudget;

/// Estimate the token cost of a single tool definition as sent to the LLM:
/// word counts for the name and description, the schema's estimated JSON
/// tokens, plus a small constant for per-tool JSON framing overhead.
fn estimate_tool_def_tokens(
    name: &str,
    description: &str,
    input_schema: &serde_json::Value,
) -> usize {
    crate::message::estimate_text_tokens(name)
        + crate::message::estimate_text_tokens(description)
        + crate::message::estimate_json_tokens(input_schema)
        + 10
}

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
    pub(super) tool_to_skill: HashMap<String, usize>,

    /// Tool definitions sent to the LLM so it knows what tools are available.
    pub(super) definitions: Vec<ToolDefinition>,

    /// Cached sum of estimated tokens for all tool definitions.
    /// Tool definitions are immutable after construction, so this is computed
    /// once and reused in `estimate_context_tokens()`.
    pub(super) cached_tokens: usize,

    /// When `true`, tool definitions are omitted from LLM requests.
    /// Set when the active model doesn't support tool use.
    pub(super) disabled: bool,
}

impl ToolRegistry {
    /// Build a tool registry by flattening all skills' tools.
    ///
    /// Duplicate tool names are handled by last-writer-wins (later skills
    /// override earlier ones), with a warning logged.
    pub(super) fn from_skills(skills: &[Box<dyn Skill>]) -> Self {
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
                    // Provider APIs require unique tool definitions.  Keep the
                    // registry and provider-facing schema list in lockstep.
                    definitions.retain(|definition| definition.name != name);
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
            .map(|t| estimate_tool_def_tokens(&t.name, &t.description, &t.input_schema))
            .sum();

        tracing::info!(tool_count = tools.len(), "tool registry built");

        Self {
            tools,
            tool_to_skill,
            definitions,
            cached_tokens,
            disabled: false,
        }
    }

    /// Look up a tool by name.
    pub(super) fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Get the skill index that owns the given tool.
    pub(super) fn skill_index(&self, tool_name: &str) -> Option<usize> {
        self.tool_to_skill.get(tool_name).copied()
    }

    /// Return tool definitions for the LLM, or `&[]` when disabled.
    pub(super) fn definitions_for_llm(&self) -> &[ToolDefinition] {
        if self.disabled {
            &[]
        } else {
            &self.definitions
        }
    }

    /// Register an extra tool not owned by any skill (e.g., advisor tool).
    pub(super) fn register_extra_tool(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        if let Some(previous) = self.definitions.iter().find(|d| d.name == name) {
            self.cached_tokens = self.cached_tokens.saturating_sub(estimate_tool_def_tokens(
                &previous.name,
                &previous.description,
                &previous.input_schema,
            ));
            self.definitions
                .retain(|definition| definition.name != name);
        }
        let tokens = estimate_tool_def_tokens(&name, tool.description(), &tool.input_schema());
        self.definitions.push(ToolDefinition {
            name: name.clone(),
            description: tool.description().to_string(),
            input_schema: tool.input_schema(),
            agent_only: tool.agent_only(),
        });
        self.tools.insert(name, tool);
        self.cached_tokens += tokens;
    }

    /// Mark tools as disabled — subsequent LLM calls will omit definitions.
    pub(super) const fn disable(&mut self) {
        self.disabled = true;
    }
}

/// Running cache for `estimate_context_tokens`: messages are immutable
/// once pushed, so only the suffix appended since the last call needs
/// estimating.  Without this, every agent-loop iteration rescanned the
/// entire history — O(n²) token counting per turn.
///
/// Invariant: `total` is the summed estimate of `messages[..counted]`.
/// Any mutation that isn't a pure append (pop, strip, compaction
/// reassembly, wholesale replacement) must call
/// [`Conversation::invalidate_token_estimates`].
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct TokenEstimateCache {
    counted: usize,
    total: usize,
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

    /// True once the iteration-budget warning has been injected for
    /// this run.  Gate to ensure we inject the synthetic user message
    /// exactly once even if the iteration counter re-enters the
    /// warning band (shouldn't happen in practice, but the flag is
    /// cheap insurance).  Reset at the start of each `run()`.
    pub(crate) budget_warning_fired: bool,

    /// Prefix cache for message token estimates.  See [`TokenEstimateCache`].
    token_estimates: TokenEstimateCache,
}

impl Conversation {
    pub(super) fn new() -> Self {
        Self {
            messages: Vec::new(),
            turn_count: 0,
            token_budget: TokenBudget::default(),
            budget_warning_fired: false,
            token_estimates: TokenEstimateCache::default(),
        }
    }

    /// Drop the cached per-message estimates.  Required after any
    /// non-append mutation of `messages`.
    pub(crate) fn invalidate_token_estimates(&mut self) {
        self.token_estimates = TokenEstimateCache::default();
    }

    /// Summed token estimate of all messages, incrementally maintained:
    /// only messages appended since the previous call are estimated.
    pub(crate) fn estimated_message_tokens(&mut self) -> usize {
        // Defensive: a shrink without invalidation means the cached
        // prefix no longer exists — recount from scratch.
        if self.token_estimates.counted > self.messages.len() {
            self.token_estimates = TokenEstimateCache::default();
        }
        for msg in &self.messages[self.token_estimates.counted..] {
            self.token_estimates.total += msg.estimate_tokens();
        }
        self.token_estimates.counted = self.messages.len();
        self.token_estimates.total
    }
}

pub(super) fn restored_turn_count(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|message| {
            matches!(message.role, Role::User)
                && !message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        })
        .count()
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
