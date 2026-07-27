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

/// Map a tool's own name onto the name providers are willing to accept.
///
/// Provider function names are constrained to `^[a-zA-Z0-9_-]{1,64}$` — the
/// pattern OpenAI documents and Anthropic matches — and a name outside it is
/// not so much rejected as silently *mangled*. Kimi reduced `a2a.send` to
/// `send` (the last dot segment), keeping the full name only inside the call
/// id, so every call dispatched as `Unknown tool 'send'` and the swarm's A2A
/// tools were unreachable from any agent on that provider.
///
/// The [`Tool`](crate::tool::Tool) trait already asks for a plain identifier,
/// but MCP servers are external and need not honour it — the swarm's own
/// first-party server advertises `a2a.send`, `agents.list` and `evals.run`. So
/// the registry keys itself on the sanitised name and resolves the raw one
/// through the same function in [`ToolRegistry::get`]. Sanitising is
/// idempotent, which is what makes that reverse step a lookup and not a guess.
fn provider_tool_name(raw: &str) -> String {
    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Every char above is ASCII, so truncating by bytes cannot split one.
    name.truncate(64);
    name
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
                // Keyed on the advertised name, so every consumer — the LLM
                // schema list, the CLI-backend MCP bridge, `tool_names()` —
                // sees exactly one entry per tool, under the name the model
                // will actually send back.
                let name = provider_tool_name(tool.name());

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
    ///
    /// Falls back to the sanitised form so a tool's own (possibly dotted) name
    /// still resolves — conversation history recorded before this change holds
    /// `tool_use` blocks naming `a2a.send`, and a provider that tolerates dots
    /// may echo the raw name back. See [`provider_tool_name`].
    pub(super) fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools
            .get(name)
            .or_else(|| self.tools.get(&provider_tool_name(name)))
    }

    /// Get the skill index that owns the given tool.
    pub(super) fn skill_index(&self, tool_name: &str) -> Option<usize> {
        self.tool_to_skill
            .get(tool_name)
            .or_else(|| self.tool_to_skill.get(&provider_tool_name(tool_name)))
            .copied()
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
        let name = provider_tool_name(tool.name());
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

#[cfg(test)]
mod tool_name_tests {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::tool::{Tool, ToolContext, ToolOutput};

    struct NamedTool(&'static str);

    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn run(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> crate::Result<ToolOutput> {
            Ok(ToolOutput::success("ok"))
        }
    }

    fn registry(names: &[&'static str]) -> ToolRegistry {
        let mut registry = ToolRegistry::from_skills(&[]);
        for name in names {
            registry.register_extra_tool(Arc::new(NamedTool(name)) as Arc<dyn Tool>);
        }
        registry
    }

    /// Provider function names are constrained to `^[a-zA-Z0-9_-]{1,64}$`
    /// (OpenAI's documented pattern, which Anthropic matches). The swarm's
    /// first-party MCP server advertises dotted names — `a2a.send`,
    /// `agents.list`, `evals.run` — and a dot is not in that set. Providers
    /// then mangle them: Kimi returned `name: "send"` (last dot segment) with
    /// the full name only in the call id, so dispatch failed with
    /// "Unknown tool 'send'" and A2A was unreachable from any agent.
    #[test]
    fn advertised_tool_names_are_provider_legal() {
        let registry = registry(&["a2a.send", "agents.list", "evals.run", "bash"]);
        for definition in registry.definitions_for_llm() {
            assert!(
                !definition.name.is_empty() && definition.name.len() <= 64,
                "{}: bad length",
                definition.name
            );
            assert!(
                definition
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "advertised name `{}` is not a legal provider function name",
                definition.name
            );
        }
    }

    /// …and the sanitised name the model was shown must dispatch back to the
    /// tool it names.
    #[test]
    fn sanitised_names_dispatch_to_their_tool() {
        let registry = registry(&["a2a.send", "agents.list", "bash"]);
        for (advertised, expected) in [
            ("a2a_send", "a2a.send"),
            ("agents_list", "agents.list"),
            ("bash", "bash"),
        ] {
            let tool = registry
                .get(advertised)
                .unwrap_or_else(|| panic!("`{advertised}` did not resolve"));
            assert_eq!(tool.name(), expected);
        }
    }

    /// The raw name stays addressable: conversation history recorded before a
    /// rename still holds `a2a.send` tool_use blocks.
    #[test]
    fn raw_dotted_names_still_resolve() {
        let registry = registry(&["a2a.send"]);
        assert_eq!(registry.get("a2a.send").unwrap().name(), "a2a.send");
    }
}
