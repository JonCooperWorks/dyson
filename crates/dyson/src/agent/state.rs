use std::collections::HashMap;
use std::sync::Arc;

use crate::chat_history::ChatHistory;
use crate::llm::ToolDefinition;
use crate::message::{ContentBlock, Message, Role};
use crate::skill::Skill;
use crate::tool::Tool;

use super::token_budget::TokenBudget;

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
        let tokens = name.split_whitespace().count()
            + tool.description().split_whitespace().count()
            + crate::message::estimate_json_tokens(&tool.input_schema())
            + 10;
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
}

impl Conversation {
    pub(super) fn new() -> Self {
        Self {
            messages: Vec::new(),
            turn_count: 0,
            token_budget: TokenBudget::default(),
            budget_warning_fired: false,
        }
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
