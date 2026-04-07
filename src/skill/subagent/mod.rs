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

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::config::{AgentSettings, LlmProvider, SubagentAgentConfig};
use crate::controller::Output;
use crate::error::{DysonError, Result};
use crate::sandbox::Sandbox;
use crate::skill::Skill;
use crate::tool::{MAX_SUBAGENT_DEPTH, Tool, ToolContext, ToolOutput};

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
    pub fn new() -> Self {
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
struct FilteredSkill {
    tools: Vec<Arc<dyn Tool>>,
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
// SubagentTool — the Tool that spawns a child agent.
// ---------------------------------------------------------------------------

/// A tool that spawns a child Agent, runs it to completion, and returns
/// the result.
///
/// Each invocation creates a **fresh** Agent with:
/// - Its own LlmClient (constructed from the subagent's provider config)
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

    /// Resolved provider type for LLM client construction.
    provider: LlmProvider,

    /// Resolved API key for the subagent's provider.
    api_key: crate::auth::Credential,

    /// Optional base URL override for the provider.
    base_url: Option<String>,

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
    /// - `api_key`: Resolved API key for the provider
    /// - `base_url`: Optional base URL override
    /// - `sandbox`: Shared sandbox from the parent
    /// - `workspace`: Shared workspace from the parent
    /// - `inherited_tools`: Pre-filtered tools from the parent
    pub fn new(
        config: SubagentAgentConfig,
        provider: LlmProvider,
        api_key: crate::auth::Credential,
        base_url: Option<String>,
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<Arc<RwLock<Box<dyn crate::workspace::Workspace>>>>,
        inherited_tools: Vec<Arc<dyn Tool>>,
    ) -> Self {
        Self {
            config,
            provider,
            api_key,
            base_url,
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
        // -- Check recursion depth --
        //
        // This is a safety net.  The primary recursion prevention is that
        // subagent tools are excluded from children's tool sets.  But depth
        // checking catches edge cases (e.g., manually constructed agents).
        if ctx.depth >= MAX_SUBAGENT_DEPTH {
            return Ok(ToolOutput::error(format!(
                "Maximum subagent nesting depth ({MAX_SUBAGENT_DEPTH}) reached. \
                 Cannot spawn another subagent."
            )));
        }

        // -- Extract task and context from input --
        let task = input["task"]
            .as_str()
            .ok_or_else(|| DysonError::tool(&self.config.name, "missing required 'task' field"))?;

        let context = input["context"].as_str().unwrap_or("");

        let user_message = if context.is_empty() {
            task.to_string()
        } else {
            format!("Context:\n{context}\n\nTask:\n{task}")
        };

        tracing::info!(
            subagent = self.config.name,
            depth = ctx.depth + 1,
            model = self.config.model.as_deref().unwrap_or("default"),
            "spawning subagent"
        );

        // -- Build the child agent's settings --
        let model = self.config.model.clone().unwrap_or_else(|| {
            crate::llm::registry::lookup(&self.provider)
                .default_model
                .to_string()
        });

        let child_settings = AgentSettings {
            model,
            max_iterations: self.config.max_iterations.unwrap_or(10),
            max_tokens: self.config.max_tokens.unwrap_or(4096),
            system_prompt: self.config.system_prompt.clone(),
            api_key: self.api_key.clone(),
            provider: self.provider.clone(),
            base_url: self.base_url.clone(),
            compaction: None,
            rate_limit: None,
        };

        // -- Create the child's LLM client --
        let client = crate::llm::create_client(
            &child_settings,
            self.workspace.clone(),
            false, // subagents don't forward dangerous_no_sandbox
        );

        // -- Build skills from inherited tools --
        let skills: Vec<Box<dyn Skill>> = vec![Box::new(FilteredSkill {
            tools: self.inherited_tools.clone(),
        })];

        // -- Create the child agent --
        let mut child_agent = crate::agent::Agent::new(
            client,
            Arc::clone(&self.sandbox),
            skills,
            &child_settings,
            self.workspace.clone(),
            0, // no nudge for subagents
        )?;

        // Set the child's depth = parent's depth + 1.
        child_agent.set_depth(ctx.depth + 1);

        // -- Run with captured output --
        let mut capture = CaptureOutput::new();
        match child_agent.run(&user_message, &mut capture).await {
            Ok(final_text) => {
                tracing::info!(
                    subagent = self.config.name,
                    result_len = final_text.len(),
                    "subagent completed successfully"
                );
                Ok(ToolOutput::success(final_text))
            }
            Err(e) => {
                tracing::warn!(
                    subagent = self.config.name,
                    error = %e,
                    "subagent failed"
                );
                Ok(ToolOutput::error(format!(
                    "Subagent '{}' failed: {e}",
                    self.config.name
                )))
            }
        }
    }
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
    ///
    /// Each config's `provider` field is looked up in `settings.providers`
    /// to resolve the provider type, API key, and base URL.
    pub fn new(
        configs: &[SubagentAgentConfig],
        settings: &crate::config::Settings,
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<Arc<RwLock<Box<dyn crate::workspace::Workspace>>>>,
        parent_tools: &[Arc<dyn Tool>],
    ) -> Self {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut prompt_lines: Vec<String> = Vec::new();

        for cfg in configs {
            // Resolve the provider.
            //
            // The special name "default" uses the parent agent's own provider,
            // API key, and base URL.  This lets built-in subagents work out of
            // the box without requiring extra provider config.
            let (provider, api_key, base_url) = if cfg.provider == "default" {
                (
                    settings.agent.provider.clone(),
                    settings.agent.api_key.clone(),
                    settings.agent.base_url.clone(),
                )
            } else {
                match settings.providers.get(&cfg.provider) {
                    Some(pc) => (
                        pc.provider_type.clone(),
                        pc.api_key.clone(),
                        pc.base_url.clone(),
                    ),
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
            let inherited = filter_tools(parent_tools, &cfg.tools);

            let tool = SubagentTool::new(
                cfg.clone(),
                provider,
                api_key,
                base_url,
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

        let has_verifier = configs.iter().any(|c| c.name == "verifier");

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

            if has_verifier {
                prompt.push_str(VERIFICATION_PROTOCOL);
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
// Verification protocol — injected into the parent's system prompt when a
// verifier subagent is available.
// ---------------------------------------------------------------------------

/// System prompt fragment that instructs the parent agent to use the
/// adversarial verification loop for non-trivial changes.
///
/// This is appended to the subagent system prompt when a "verifier"
/// subagent is present.  It defines what "non-trivial" means, when to
/// invoke the verifier, and how to handle its verdicts.
const VERIFICATION_PROTOCOL: &str = "\n\n\
## Verification Protocol\n\n\
For non-trivial changes, you MUST use the `verifier` subagent before \
reporting completion.  A change is non-trivial if ANY of these apply:\n\
- You edited 3 or more files.\n\
- You changed backend, API, or infrastructure logic.\n\
- You modified configuration or build files.\n\n\
### Verify-Before-Report Loop\n\n\
1. **Implement** the change.\n\
2. **Spawn the verifier** with:\n\
   - `task`: A description of what to verify.\n\
   - `context`: The original user request, the list of files changed, \
     and the approach taken.\n\
3. **Read the verdict**:\n\
   - **PASS** → You may report completion.  Before doing so, independently \
     run 2–3 of the commands the verifier reported to spot-check its results.\n\
   - **FAIL** → Fix every issue the verifier identified, then re-invoke the \
     verifier with the updated changes.  Repeat until PASS.\n\
   - **PARTIAL** → Fix the failing components and re-invoke the verifier.  \
     Repeat until PASS.\n\
4. **Never self-certify**.  Only the verifier can issue a PASS verdict for \
   non-trivial changes.  Do not skip verification or report success without it.\n";

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
            system_prompt: "You are a planning specialist.  Your job is to analyze a task \
                and break it into concrete, ordered implementation steps.\n\n\
                Rules:\n\
                1. Read relevant files to understand the codebase structure before planning.\n\
                2. Each step must be specific — include file paths, function names, and what \
                   to change.\n\
                3. Order steps by dependency — what must happen first.\n\
                4. Identify risks or decisions that need human input.\n\
                5. Keep the plan concise — no filler, just actionable steps.\n\
                6. Do NOT implement anything.  Only plan."
                .into(),
            provider: "default".into(),
            model: None,
            max_iterations: Some(15),
            max_tokens: Some(4096),
            tools: Some(vec![
                "read_file".into(),
                "search_files".into(),
                "list_files".into(),
            ]),
        },
        SubagentAgentConfig {
            name: "researcher".into(),
            description: "Does deep research and summarizes findings.  Can read code, \
                run commands, and search the web.  Returns a concise summary of what \
                it found.  Use for questions that need investigation."
                .into(),
            system_prompt: "You are a research specialist.  Your job is to thoroughly \
                investigate a question and return a clear, concise summary.\n\n\
                Rules:\n\
                1. Use your tools to gather information — read files, run commands, \
                   search the web.\n\
                2. Be thorough — check multiple sources when possible.\n\
                3. Cite specifics — file paths, line numbers, URLs.\n\
                4. Summarize findings clearly — lead with the answer, then supporting \
                   evidence.\n\
                5. Flag uncertainty — if you're not sure, say so."
                .into(),
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
            system_prompt: "You are an adversarial verification specialist.  Your sole \
                objective is to find bugs, regressions, and spec violations in a proposed \
                change.  You are NOT here to help — you are here to break things.\n\n\
                ## Protocol\n\n\
                1. Read the original request and the list of changed files.\n\
                2. Read every changed file.  Understand what was done.\n\
                3. Attempt to falsify the implementation:\n\
                   - Run the project's test suite (look for Makefile, Cargo.toml, \
                     package.json, etc.).\n\
                   - Run linters or type checkers if available.\n\
                   - Test edge cases by reading code paths and reasoning about inputs.\n\
                   - Check for regressions: did the change break existing functionality?\n\
                   - Verify the change actually satisfies the original request.\n\
                4. For every command you run, record the exact command and its output.\n\n\
                ## Verdict Format\n\n\
                You MUST end your response with exactly one of these verdicts:\n\n\
                **VERDICT: PASS**\n\
                The implementation meets the spec and all checks pass.\n\n\
                **VERDICT: FAIL**\n\
                One or more checks failed.  List each failure with:\n\
                - What failed\n\
                - The command that demonstrated the failure\n\
                - The relevant output\n\n\
                **VERDICT: PARTIAL**\n\
                Some components work, others fail.  List what passes and what fails \
                using the same format as FAIL.\n\n\
                ## Rules\n\n\
                1. Your goal is to find a FAIL condition.  Only issue PASS if you \
                   genuinely cannot break the implementation.\n\
                2. You must provide proof of execution — exact commands and their \
                   output — for every check.\n\
                3. Do NOT fix anything.  Only verify and report.\n\
                4. Do NOT be lenient.  Assume the implementation is wrong until \
                   proven otherwise.\n\
                5. Check compilation/build first — if it doesn't build, nothing \
                   else matters."
                .into(),
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

// ===========================================================================
// Test support — public re-exports for integration tests.
// ===========================================================================

/// Public test helpers for integration tests in `tests/subagent_eval.rs`.
///
/// These types are implementation details of the subagent system, but
/// integration tests need direct access to construct child agents with
/// FilteredSkill and CaptureOutput.  This module re-exports them under
/// a clearly-marked test-support namespace.
#[doc(hidden)]
pub mod tests_support {
    use super::*;

    /// Public wrapper around `FilteredSkill` for integration tests.
    ///
    /// Identical to the private `FilteredSkill` — just publicly accessible.
    pub struct FilteredSkillPublic {
        inner: FilteredSkill,
    }

    impl FilteredSkillPublic {
        pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
            Self {
                inner: FilteredSkill { tools },
            }
        }
    }

    #[async_trait]
    impl Skill for FilteredSkillPublic {
        fn name(&self) -> &str {
            self.inner.name()
        }

        fn tools(&self) -> &[Arc<dyn Tool>] {
            self.inner.tools()
        }
    }
}

#[cfg(test)]
mod tests;
