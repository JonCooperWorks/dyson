// ===========================================================================
// Subagent skill — spawn child agents as tools.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements "subagents" — child agents that the parent LLM can invoke
//   as tools.  Each subagent has its own LlmClient (potentially a different
//   model/provider), its own system prompt, and its own conversation history.
//   When invoked, the subagent runs to completion and returns its final text
//   as a ToolOutput.
//
// Why subagents?
//   Different tasks benefit from different models.  A Claude parent might
//   delegate research to a GPT subagent, or a fast model might delegate
//   complex reasoning to a slower, more capable one.  Subagents enable
//   this delegation pattern while maintaining security (shared sandbox)
//   and memory (shared workspace).
//
// Architecture:
//
//   Parent Agent (e.g., Claude Sonnet)
//     │
//     ├── bash, read_file, ...          ← normal tools
//     ├── research_agent (SubagentTool) ← spawns child on invocation
//     │     │
//     │     ▼
//     │   Child Agent (e.g., GPT-4o)
//     │     ├── bash, read_file, ...    ← inherited from parent
//     │     └── (runs to completion)
//     │     │
//     │     ▼
//     │   returns final text → ToolOutput
//     │
//     └── code_review_agent (SubagentTool) ← another subagent
//
// Key design decisions:
//
//   1. Shared sandbox: Subagents share the parent's sandbox via Arc<dyn
//      Sandbox>.  This is non-negotiable — a subagent must not be able to
//      bypass the parent's security policy.
//
//   2. Shared workspace: Subagents share the parent's workspace so they
//      can read/write the same memory files.  This enables collaboration
//      between the parent and its subagents.
//
//   3. Inherited tools: Subagents get the parent's loaded tools (builtins,
//      MCP, local skills) via Arc<dyn Tool> clones — no duplication, no
//      reconnecting to MCP servers.  The optional `tools` config filter
//      restricts which tools are visible.
//
//   4. Conversation isolation: Each subagent invocation starts with a
//      fresh conversation.  The child's internal messages never leak into
//      the parent's history — only the final text does.
//
//   5. Recursion depth limit: ToolContext carries a `depth` counter.
//      Subagent tools themselves are excluded from children, and depth
//      is checked as a safety net (MAX_SUBAGENT_DEPTH = 3).
//
//   6. Output capture: A CaptureOutput collects the child's streaming
//      text into a String instead of printing to the terminal.
//
// Module contents:
//   CaptureOutput    — Output impl that captures text into a String
//   SubagentTool     — Tool impl that spawns a child Agent per invocation
//   FilteredSkill    — Skill impl wrapping pre-loaded tools for the child
//   SubagentSkill    — Skill impl that bundles SubagentTool instances
// ===========================================================================

mod coder;

pub use coder::CoderTool;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::agent::rate_limiter::RateLimitedHandle;
use crate::config::{AgentSettings, LlmProvider, SubagentAgentConfig};
use crate::controller::Output;
use crate::error::{DysonError, Result};
use crate::llm::LlmClient;
use crate::sandbox::Sandbox;
use crate::skill::Skill;
use crate::tool::{MAX_SUBAGENT_DEPTH, Tool, ToolContext, ToolOutput};
use crate::workspace::Workspace;

// ---------------------------------------------------------------------------
// CaptureOutput — collects agent output into a String.
// ---------------------------------------------------------------------------

/// An `Output` implementation that captures text into a buffer.
///
/// Used by subagents to collect their streaming output without printing
/// to the terminal.  The parent agent only sees the final text via
/// `ToolOutput`, not the intermediate streaming events.
///
/// Tool events (tool_use_start, tool_result) are logged for debugging
/// but not included in the captured text — only the LLM's natural
/// language output matters for the parent.
#[derive(Default)]
pub struct CaptureOutput {
    /// Accumulated text from TextDelta events.
    text: String,
}

impl CaptureOutput {
    /// Create a new empty capture buffer.
    pub const fn new() -> Self {
        Self {
            text: String::new(),
        }
    }

    /// Get the accumulated text.
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Output for CaptureOutput {
    fn text_delta(&mut self, text: &str) -> Result<()> {
        self.text.push_str(text);
        Ok(())
    }

    fn tool_use_start(&mut self, _id: &str, name: &str) -> Result<()> {
        tracing::debug!(tool = name, "subagent tool call started");
        Ok(())
    }

    fn tool_use_complete(&mut self) -> Result<()> {
        Ok(())
    }

    fn tool_result(&mut self, output: &ToolOutput) -> Result<()> {
        tracing::debug!(
            is_error = output.is_error,
            content_len = output.content.len(),
            "subagent tool result"
        );
        Ok(())
    }

    fn send_file(&mut self, path: &std::path::Path) -> Result<()> {
        tracing::debug!(path = %path.display(), "subagent file send (ignored in capture)");
        Ok(())
    }

    fn error(&mut self, error: &DysonError) -> Result<()> {
        tracing::warn!(error = %error, "subagent error");
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FilteredSkill — wraps pre-loaded tools for the child agent.
// ---------------------------------------------------------------------------

/// A lightweight Skill wrapper around a set of pre-loaded tools.
///
/// Used to pass inherited parent tools to the child agent.  Unlike
/// BuiltinSkill or McpSkill, this doesn't create or own tools — it
/// wraps existing `Arc<dyn Tool>` pointers.  No lifecycle hooks are
/// needed because the tools are already initialized by the parent's
/// skills.
///
/// This type is public for use in integration tests
/// (`tests/subagent_eval.rs`) but is not part of the stable public API.
pub struct FilteredSkill {
    tools: Vec<Arc<dyn Tool>>,
}

impl FilteredSkill {
    /// Construct a `FilteredSkill` from pre-loaded tools.
    ///
    /// Exposed for integration tests that build a child agent manually;
    /// production code goes through `SubagentSkill::new` or `CoderTool`,
    /// both of which wrap `FilteredSkill` internally.
    #[doc(hidden)]
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { tools }
    }
}

#[async_trait]
impl Skill for FilteredSkill {
    fn name(&self) -> &str {
        "inherited"
    }

    fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }
}

// ---------------------------------------------------------------------------
// spawn_child — shared helper used by SubagentTool and CoderTool.
// ---------------------------------------------------------------------------

/// Arguments for [`spawn_child`].
///
/// Bundles the configuration needed to construct a child agent from a
/// parent tool call.  Both `SubagentTool` and `CoderTool` build one of
/// these and delegate to `spawn_child`, which owns the common lifecycle
/// (depth check, builder, workspace wiring, capture, tracing).
pub(crate) struct ChildSpawn<'a> {
    /// Tool name for logging and error formatting.
    pub name: &'a str,
    /// Resolved agent settings for the child (model, prompt, limits).
    pub settings: AgentSettings,
    /// Tools the child inherits — cloned into a `FilteredSkill`.
    pub inherited_tools: Vec<Arc<dyn Tool>>,
    /// Shared sandbox from the parent agent.
    pub sandbox: Arc<dyn Sandbox>,
    /// Shared workspace from the parent agent.
    pub workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,
    /// Pre-authenticated client handle from `ClientRegistry`.
    pub client: RateLimitedHandle<Box<dyn LlmClient>>,
    /// Parent depth — child runs at `parent_depth + 1`.
    pub parent_depth: u8,
    /// Optional working-directory override (used by `CoderTool`).
    pub working_dir: Option<PathBuf>,
    /// First user message to the child agent.
    pub user_message: String,
}

/// Spawn a child agent, run it to completion, and return its final text
/// as a `ToolOutput`.
///
/// Consolidates the duplicate child-spawn logic that previously lived in
/// both `SubagentTool::run` and `CoderTool::run`.  Handles the depth
/// guard, `FilteredSkill` wrapping, `Agent::builder` construction,
/// workspace wiring, `CaptureOutput` setup, and the success/failure
/// tracing path.
///
/// Depth overflow is reported as `ToolOutput::error` (a recoverable
/// tool failure) rather than an `Err`, matching the existing pattern.
pub(crate) async fn spawn_child(spec: ChildSpawn<'_>) -> Result<ToolOutput> {
    // -- Safety net: enforce recursion depth --
    //
    // The primary recursion prevention is that subagent tools are
    // excluded from children's tool sets.  Depth is a belt-and-suspenders
    // check for manually constructed agents.
    if spec.parent_depth >= MAX_SUBAGENT_DEPTH {
        return Ok(ToolOutput::error(format!(
            "Maximum subagent nesting depth ({MAX_SUBAGENT_DEPTH}) reached. \
             Cannot spawn another subagent."
        )));
    }

    tracing::info!(
        tool = spec.name,
        depth = spec.parent_depth + 1,
        model = spec.settings.model.as_str(),
        "spawning child agent"
    );

    let skills: Vec<Box<dyn Skill>> =
        vec![Box::new(FilteredSkill::new(spec.inherited_tools))];

    let mut builder = crate::agent::Agent::builder(spec.client, spec.sandbox)
        .skills(skills)
        .settings(&spec.settings);
    if let Some(ws) = spec.workspace {
        builder = builder.workspace(ws);
    }
    let mut child_agent = builder.build()?;

    child_agent.set_depth(spec.parent_depth + 1);
    if let Some(dir) = spec.working_dir {
        child_agent.set_working_dir(dir);
    }

    let mut capture = CaptureOutput::new();
    match child_agent.run(&spec.user_message, &mut capture).await {
        Ok(final_text) => {
            tracing::info!(
                tool = spec.name,
                result_len = final_text.len(),
                "child agent completed successfully"
            );
            Ok(ToolOutput::success(final_text))
        }
        Err(e) => {
            tracing::warn!(
                tool = spec.name,
                error = %e,
                "child agent failed"
            );
            Ok(ToolOutput::error(format!(
                "Subagent '{}' failed: {e}",
                spec.name,
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// SubagentTool — the Tool that spawns a child agent.
// ---------------------------------------------------------------------------

/// A tool that spawns a child Agent, runs it to completion, and returns
/// the result.
///
/// Each invocation creates a child Agent with:
/// - A shared LLM client handle (from `ClientRegistry`, same rate limits)
/// - Its own conversation history (empty — no context leak from parent)
/// - The parent's sandbox (shared via Arc — security cannot be bypassed)
/// - The parent's workspace (shared via Arc — memory is collaborative)
/// - A filtered subset of the parent's tools (via FilteredSkill)
///
/// ## Input schema
///
/// ```json
/// {
///   "task": "Research the latest Rust async patterns",
///   "context": "We're building a streaming agent framework"
/// }
/// ```
///
/// - `task` (required): What the subagent should do.
/// - `context` (optional): Background information to help the subagent.
///
/// ## Security
///
/// The subagent runs through the same sandbox as the parent.  If the
/// parent's sandbox denies `rm -rf /`, so does the child's.  This is
/// enforced by sharing `Arc<dyn Sandbox>` — there's no way to construct
/// a SubagentTool without a sandbox reference.
pub struct SubagentTool {
    /// Configuration for this subagent (name, description, provider, etc.).
    config: SubagentAgentConfig,

    /// Resolved provider type (kept for system prompt injection).
    provider: LlmProvider,

    /// Shared LLM client handle — from the same `ClientRegistry` as the
    /// parent agent.  Shares the rate-limit window so subagents can't
    /// bypass the provider's rate limits.
    client: crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>,

    /// Shared sandbox — same instance as the parent agent.
    sandbox: Arc<dyn Sandbox>,

    /// Shared workspace — same instance as the parent agent.
    workspace: Option<Arc<RwLock<Box<dyn crate::workspace::Workspace>>>>,

    /// Tools inherited from the parent, filtered by config.
    ///
    /// These are `Arc<dyn Tool>` clones from the parent's already-loaded
    /// skills.  No duplication — just shared pointers.  MCP tools work
    /// seamlessly because the MCP connection is owned by the parent's
    /// McpSkill, and the Arc<dyn Tool> just forwards calls to it.
    inherited_tools: Vec<Arc<dyn Tool>>,
}

impl SubagentTool {
    /// Construct a new SubagentTool.
    ///
    /// ## Parameters
    ///
    /// - `config`: Per-subagent settings (name, description, provider, etc.)
    /// - `provider`: Resolved LlmProvider enum variant
    /// - `client`: Shared LLM client handle (from `ClientRegistry`)
    /// - `sandbox`: Shared sandbox from the parent
    /// - `workspace`: Shared workspace from the parent
    /// - `inherited_tools`: Pre-filtered tools from the parent
    pub fn new(
        config: SubagentAgentConfig,
        provider: LlmProvider,
        client: crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>,
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<Arc<RwLock<Box<dyn crate::workspace::Workspace>>>>,
        inherited_tools: Vec<Arc<dyn Tool>>,
    ) -> Self {
        Self {
            config,
            provider,
            client,
            sandbox,
            workspace,
            inherited_tools,
        }
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn description(&self) -> &str {
        &self.config.description
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task for the subagent to complete."
                },
                "context": {
                    "type": "string",
                    "description": "Optional background context to help the subagent understand the task."
                }
            },
            "required": ["task"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let parsed: SubagentInput = serde_json::from_value(input.clone()).map_err(|e| {
            DysonError::tool(&self.config.name, format!("invalid input: {e}"))
        })?;

        let user_message = if parsed.context.is_empty() {
            parsed.task
        } else {
            format!("Context:\n{}\n\nTask:\n{}", parsed.context, parsed.task)
        };

        let model = self.config.model.clone().unwrap_or_else(|| {
            crate::llm::registry::lookup(&self.provider)
                .default_model
                .to_string()
        });

        let settings = AgentSettings {
            model,
            max_iterations: self.config.max_iterations.unwrap_or(10),
            max_tokens: self.config.max_tokens.unwrap_or(4096),
            system_prompt: self.config.system_prompt.clone(),
            provider: self.provider.clone(),
            // api_key/base_url are unused — the client handle is pre-authenticated.
            ..AgentSettings::default()
        };

        spawn_child(ChildSpawn {
            name: &self.config.name,
            settings,
            inherited_tools: self.inherited_tools.clone(),
            sandbox: Arc::clone(&self.sandbox),
            workspace: self.workspace.clone(),
            client: self.client.clone(),
            parent_depth: ctx.depth,
            working_dir: None,
            user_message,
        })
        .await
    }
}

/// Parsed input payload for `SubagentTool`.
///
/// Mirrors the tool's JSON input schema (`task` required, `context`
/// optional).  Using serde here gives us field-level error messages for
/// free and eliminates the repetitive `input["task"].as_str()` dance.
#[derive(Debug, Deserialize)]
struct SubagentInput {
    task: String,
    #[serde(default)]
    context: String,
}

// ---------------------------------------------------------------------------
// SubagentSkill — bundles SubagentTool instances into a Skill.
// ---------------------------------------------------------------------------

/// A skill that provides one or more subagent tools to the parent agent.
///
/// Constructed **after** all other skills are loaded, so it can clone
/// `Arc<dyn Tool>` pointers from the already-initialized parent tools.
/// This two-phase construction avoids the chicken-and-egg problem: we
/// need the parent's tools to exist before we can give them to subagents.
///
/// ## System prompt
///
/// The skill contributes a system prompt fragment that describes the
/// available subagents so the parent LLM knows when to delegate.
pub struct SubagentSkill {
    /// The subagent tools, stored as Arc for shared ownership with the agent.
    tools: Vec<Arc<dyn Tool>>,

    /// System prompt fragment describing available subagents.
    system_prompt: String,
}

impl SubagentSkill {
    /// Create a SubagentSkill from resolved configurations.
    ///
    /// ## Parameters
    ///
    /// - `configs`: Per-subagent configurations from dyson.json
    /// - `settings`: Full settings (for resolving provider references)
    /// - `sandbox`: Shared sandbox from the parent
    /// - `workspace`: Shared workspace from the parent
    /// - `parent_tools`: All tools loaded by the parent's other skills
    /// - `registry`: Shared client registry for obtaining LLM client handles
    ///
    /// Each config's `provider` field is looked up in `registry` to obtain
    /// a shared client handle.  The "default" provider uses the parent's
    /// active provider from the registry.
    pub fn new(
        configs: &[SubagentAgentConfig],
        settings: &crate::config::Settings,
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<Arc<RwLock<Box<dyn crate::workspace::Workspace>>>>,
        parent_tools: &[Arc<dyn Tool>],
        registry: &crate::controller::ClientRegistry,
    ) -> Self {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut prompt_lines: Vec<String> = Vec::new();

        for cfg in configs {
            // Resolve the provider and get a shared client handle.
            //
            // The special name "default" uses the parent agent's own provider.
            // This lets built-in subagents work out of the box without
            // requiring extra provider config.
            let (provider, client) = if cfg.provider == "default" {
                (settings.agent.provider.clone(), registry.get_default())
            } else {
                match settings.providers.get(&cfg.provider) {
                    Some(pc) => match registry.get(&cfg.provider) {
                        Ok(handle) => (pc.provider_type.clone(), handle),
                        Err(e) => {
                            tracing::error!(
                                subagent = cfg.name,
                                provider = cfg.provider,
                                error = %e,
                                "failed to create client for subagent — skipping"
                            );
                            continue;
                        }
                    },
                    None => {
                        tracing::error!(
                            subagent = cfg.name,
                            provider = cfg.provider,
                            "unknown provider for subagent — skipping"
                        );
                        continue;
                    }
                }
            };

            // Filter the parent's tools for this subagent.
            let inherited = filter_tools_checked(&cfg.name, parent_tools, &cfg.tools);

            let tool = SubagentTool::new(
                cfg.clone(),
                provider,
                client,
                Arc::clone(&sandbox),
                workspace.clone(),
                inherited,
            );

            let tool_names: Vec<&str> = tool.inherited_tools.iter().map(|t| t.name()).collect();
            tracing::info!(
                subagent = cfg.name,
                provider = cfg.provider,
                tools = ?tool_names,
                "subagent configured"
            );

            prompt_lines.push(format!("- **{}**: {}", cfg.name, cfg.description,));

            tools.push(Arc::new(tool));
        }

        // -- Coder tool (built-in, always present) --
        //
        // Unlike config-driven subagents, the coder has a custom struct
        // because it accepts a `path` parameter and scopes the child
        // agent's working_dir — behavior SubagentTool doesn't support.
        let (coder_provider, coder_client) =
            (settings.agent.provider.clone(), registry.get_default());
        let coder_tool = CoderTool::new(
            coder_provider,
            coder_client,
            Arc::clone(&sandbox),
            workspace.clone(),
            parent_tools,
        );
        prompt_lines.push(format!("- **{}**: {}", coder_tool.name(), coder_tool.description()));
        tools.push(Arc::new(coder_tool));

        // Collect protocol fragments from subagents that bring their own
        // usage instructions (e.g., `verifier` defines when it must be
        // invoked).  This replaces the earlier magic-string check on the
        // subagent name.
        let protocol_fragments: Vec<&str> = configs
            .iter()
            .filter_map(|c| c.injects_protocol.as_deref())
            .collect();

        let system_prompt = if prompt_lines.is_empty() {
            String::new()
        } else {
            let mut prompt = format!(
                "You have access to the following subagents — specialized AI agents \
                 that you can delegate tasks to.  Each subagent may use a different \
                 model and has its own expertise.  Use them when their specialty \
                 matches the task:\n\n{}",
                prompt_lines.join("\n")
            );

            for fragment in protocol_fragments {
                prompt.push_str(fragment);
            }

            prompt
        };

        Self {
            tools,
            system_prompt,
        }
    }
}

#[async_trait]
impl Skill for SubagentSkill {
    fn name(&self) -> &str {
        "subagents"
    }

    fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    fn system_prompt(&self) -> Option<&str> {
        if self.system_prompt.is_empty() {
            None
        } else {
            Some(&self.system_prompt)
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in subagent configurations.
// ---------------------------------------------------------------------------

/// Returns the default built-in subagent configurations.
///
/// These ship with every Dyson instance and use the `"default"` provider
/// (the parent agent's own provider).  Users can override or extend these
/// by adding their own subagent configs in dyson.json.
///
/// Built-in subagents:
///
/// - **planner**: Breaks down complex tasks into concrete, ordered steps.
///   Given read-only tools so it can inspect the codebase before planning.
///   Use it before tackling multi-step work.
///
/// - **researcher**: Does deep research and summarizes findings.  Has
///   broader tool access including bash and web_search for thorough
///   investigation.  Use it for questions that need exploration.
pub fn builtin_subagent_configs() -> Vec<SubagentAgentConfig> {
    vec![
        SubagentAgentConfig {
            name: "planner".into(),
            description: "Breaks down complex tasks into concrete, ordered implementation \
                steps.  Reads the codebase to understand structure before planning.  \
                Returns a numbered plan with file paths and specific changes needed."
                .into(),
            system_prompt: include_str!("prompts/planner.md").into(),
            provider: "default".into(),
            model: None,
            max_iterations: Some(15),
            max_tokens: Some(4096),
            tools: Some(vec![
                "read_file".into(),
                "search_files".into(),
                "list_files".into(),
            ]),
            injects_protocol: None,
        },
        SubagentAgentConfig {
            name: "researcher".into(),
            description: "Does deep research and summarizes findings.  Can read code, \
                run commands, and search the web.  Returns a concise summary of what \
                it found.  Use for questions that need investigation."
                .into(),
            system_prompt: include_str!("prompts/researcher.md").into(),
            provider: "default".into(),
            model: None,
            max_iterations: Some(20),
            max_tokens: Some(4096),
            tools: Some(vec![
                "bash".into(),
                "read_file".into(),
                "search_files".into(),
                "list_files".into(),
                "web_search".into(),
            ]),
            injects_protocol: None,
        },
        SubagentAgentConfig {
            name: "verifier".into(),
            description: "Adversarial verification agent.  Attempts to break an \
                implementation by running tests, checking edge cases, and validating \
                that changes meet the original specification.  Returns a structured \
                verdict: PASS, FAIL, or PARTIAL with proof of execution.  Use after \
                completing non-trivial changes (3+ files, backend/API logic, or \
                infrastructure modifications)."
                .into(),
            system_prompt: include_str!("prompts/verifier.md").into(),
            provider: "default".into(),
            model: None,
            max_iterations: Some(25),
            max_tokens: Some(8192),
            tools: Some(vec![
                "bash".into(),
                "read_file".into(),
                "search_files".into(),
                "list_files".into(),
            ]),
            // When `verifier` is present, parent agents are told how to
            // invoke it and interpret its verdict.  Data-driven so any
            // user-defined subagent can inject its own protocol.
            injects_protocol: Some(include_str!("prompts/verifier_protocol.md").into()),
        },
    ]
}

// ---------------------------------------------------------------------------
// Tool filtering helper
// ---------------------------------------------------------------------------

/// Filter parent tools based on the subagent's `tools` config.
///
/// - If `filter` is `None`: inherit all parent tools (subagent tools are
///   already excluded by the caller since they haven't been created yet
///   during two-phase construction).
/// - If `filter` is `Some(names)`: only include tools whose names are in
///   the list.  Unknown names are silently ignored.
fn filter_tools(
    parent_tools: &[Arc<dyn Tool>],
    filter: &Option<Vec<String>>,
) -> Vec<Arc<dyn Tool>> {
    match filter {
        None => parent_tools.to_vec(),
        Some(names) => parent_tools
            .iter()
            .filter(|t| names.iter().any(|n| n == t.name()))
            .cloned()
            .collect(),
    }
}

/// Like [`filter_tools`] but also emits a `warn!` log for any filter
/// entry that doesn't match a real parent tool.  Used during
/// `SubagentSkill::new` so operators see why a configured tool is
/// missing instead of silently losing it.
fn filter_tools_checked(
    subagent_name: &str,
    parent_tools: &[Arc<dyn Tool>],
    filter: &Option<Vec<String>>,
) -> Vec<Arc<dyn Tool>> {
    if let Some(names) = filter {
        let known: std::collections::HashSet<&str> =
            parent_tools.iter().map(|t| t.name()).collect();
        let missing: Vec<&str> = names
            .iter()
            .map(String::as_str)
            .filter(|n| !known.contains(n))
            .collect();
        if !missing.is_empty() {
            tracing::warn!(
                subagent = subagent_name,
                missing = ?missing,
                "subagent tool filter references unknown tools — they will be dropped"
            );
        }
    }
    filter_tools(parent_tools, filter)
}

#[cfg(test)]
mod tests;
