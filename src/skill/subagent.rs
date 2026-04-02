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

        let system_prompt = if prompt_lines.is_empty() {
            String::new()
        } else {
            format!(
                "You have access to the following subagents — specialized AI agents \
                 that you can delegate tasks to.  Each subagent may use a different \
                 model and has its own expertise.  Use them when their specialty \
                 matches the task:\n\n{}",
                prompt_lines.join("\n")
            )
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

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::stream::{StopReason, StreamEvent};

    // -----------------------------------------------------------------------
    // CaptureOutput tests
    // -----------------------------------------------------------------------

    #[test]
    fn capture_output_collects_text_deltas() {
        let mut output = CaptureOutput::new();
        output.text_delta("Hello, ").unwrap();
        output.text_delta("world!").unwrap();
        assert_eq!(output.text(), "Hello, world!");
    }

    #[test]
    fn capture_output_starts_empty() {
        let output = CaptureOutput::new();
        assert_eq!(output.text(), "");
    }

    #[test]
    fn capture_output_handles_tool_events() {
        let mut output = CaptureOutput::new();
        output.tool_use_start("id_1", "bash").unwrap();
        output.tool_use_complete().unwrap();
        output.tool_result(&ToolOutput::success("result")).unwrap();
        // Tool events should not add to the captured text.
        assert_eq!(output.text(), "");
    }

    #[test]
    fn capture_output_handles_errors() {
        let mut output = CaptureOutput::new();
        output.error(&DysonError::Llm("test error".into())).unwrap();
        // Errors are logged, not captured as text.
        assert_eq!(output.text(), "");
    }

    #[test]
    fn capture_output_handles_flush() {
        let mut output = CaptureOutput::new();
        output.text_delta("text").unwrap();
        output.flush().unwrap();
        assert_eq!(output.text(), "text");
    }

    #[test]
    fn capture_output_ignores_file_sends() {
        let mut output = CaptureOutput::new();
        output
            .send_file(std::path::Path::new("/tmp/test.pdf"))
            .unwrap();
        assert_eq!(output.text(), "");
    }

    // -----------------------------------------------------------------------
    // SubagentTool metadata tests
    // -----------------------------------------------------------------------

    #[test]
    fn subagent_tool_name_and_description() {
        let config = SubagentAgentConfig {
            name: "research_agent".into(),
            description: "Research specialist".into(),
            system_prompt: "You are a researcher.".into(),
            provider: "anthropic".into(),
            model: None,
            max_iterations: None,
            max_tokens: None,
            tools: None,
        };

        let tool = SubagentTool::new(
            config,
            LlmProvider::Anthropic,
            crate::auth::Credential::new(String::new()),
            None,
            Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
            None,
            vec![],
        );

        assert_eq!(tool.name(), "research_agent");
        assert_eq!(tool.description(), "Research specialist");
    }

    #[test]
    fn subagent_tool_input_schema_has_required_task() {
        let config = SubagentAgentConfig {
            name: "test_agent".into(),
            description: "Test".into(),
            system_prompt: "Test".into(),
            provider: "anthropic".into(),
            model: None,
            max_iterations: None,
            max_tokens: None,
            tools: None,
        };

        let tool = SubagentTool::new(
            config,
            LlmProvider::Anthropic,
            crate::auth::Credential::new(String::new()),
            None,
            Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
            None,
            vec![],
        );

        let schema = tool.input_schema();
        assert_eq!(schema["properties"]["task"]["type"], "string");
        assert_eq!(schema["properties"]["context"]["type"], "string");
        assert_eq!(schema["required"][0], "task");
    }

    // -----------------------------------------------------------------------
    // SubagentTool execution tests (with MockLlm)
    // -----------------------------------------------------------------------

    /// Mock LLM that returns pre-programmed responses for subagent tests.
    struct MockLlm {
        responses: std::sync::Mutex<Vec<Vec<StreamEvent>>>,
    }

    impl MockLlm {
        fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl crate::llm::LlmClient for MockLlm {
        async fn stream(
            &self,
            _messages: &[crate::message::Message],
            _system: &str,
            _tools: &[crate::llm::ToolDefinition],
            _config: &crate::llm::CompletionConfig,
        ) -> Result<crate::llm::StreamResponse> {
            let events = self.responses.lock().unwrap().remove(0);
            Ok(crate::llm::StreamResponse {
                stream: Box::pin(tokio_stream::iter(events.into_iter().map(Ok))),
                tool_mode: crate::llm::ToolMode::Execute,
                input_tokens: None,
            })
        }
    }

    /// Helper to create a SubagentTool that uses a MockLlm.
    ///
    /// Since SubagentTool creates its own LlmClient internally via
    /// `crate::llm::create_client()`, we can't inject a MockLlm directly.
    /// Instead, we test the full flow by constructing a child Agent
    /// manually and running it — this tests the same code path that
    /// SubagentTool::run() uses.
    #[tokio::test]
    async fn subagent_runs_child_and_returns_result() {
        // Build a mock child agent that returns "Research complete."
        let llm = MockLlm::new(vec![vec![
            StreamEvent::TextDelta("Research complete.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ]]);

        let settings = AgentSettings {
            api_key: "test".into(),
            system_prompt: "You are a researcher.".into(),
            ..Default::default()
        };

        let skills: Vec<Box<dyn Skill>> = vec![Box::new(FilteredSkill { tools: vec![] })];
        let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
        let mut agent =
            crate::agent::Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();
        agent.set_depth(1);

        let mut capture = CaptureOutput::new();
        let result = agent
            .run("Research Rust patterns", &mut capture)
            .await
            .unwrap();

        assert_eq!(result, "Research complete.");
        assert_eq!(capture.text(), "Research complete.");
    }

    #[tokio::test]
    async fn subagent_depth_limit_prevents_recursion() {
        let config = SubagentAgentConfig {
            name: "deep_agent".into(),
            description: "Too deep".into(),
            system_prompt: "Test".into(),
            provider: "anthropic".into(),
            model: None,
            max_iterations: None,
            max_tokens: None,
            tools: None,
        };

        let tool = SubagentTool::new(
            config,
            LlmProvider::Anthropic,
            crate::auth::Credential::new(String::new()),
            None,
            Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
            None,
            vec![],
        );

        // Create a context at max depth.
        let ctx = ToolContext {
            working_dir: std::env::current_dir().unwrap(),
            env: std::collections::HashMap::new(),
            cancellation: tokio_util::sync::CancellationToken::new(),
            workspace: None,
            depth: MAX_SUBAGENT_DEPTH,
        };

        let input = serde_json::json!({"task": "should fail"});
        let result = tool.run(&input, &ctx).await.unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("Maximum subagent nesting depth"));
    }

    #[tokio::test]
    async fn subagent_missing_task_returns_error() {
        let config = SubagentAgentConfig {
            name: "test_agent".into(),
            description: "Test".into(),
            system_prompt: "Test".into(),
            provider: "anthropic".into(),
            model: None,
            max_iterations: None,
            max_tokens: None,
            tools: None,
        };

        let tool = SubagentTool::new(
            config,
            LlmProvider::Anthropic,
            crate::auth::Credential::new(String::new()),
            None,
            Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
            None,
            vec![],
        );

        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({}); // No "task" field

        let result = tool.run(&input, &ctx).await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // FilteredSkill tests
    // -----------------------------------------------------------------------

    #[test]
    fn filtered_skill_exposes_tools() {
        let tool: Arc<dyn Tool> = Arc::new(crate::tool::bash::BashTool::default());
        let skill = FilteredSkill { tools: vec![tool] };

        assert_eq!(skill.name(), "inherited");
        assert_eq!(skill.tools().len(), 1);
        assert_eq!(skill.tools()[0].name(), "bash");
    }

    // -----------------------------------------------------------------------
    // SubagentSkill tests
    // -----------------------------------------------------------------------

    #[test]
    fn subagent_skill_system_prompt_lists_agents() {
        // Create a minimal settings with a provider.
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "claude".to_string(),
            crate::config::ProviderConfig {
                provider_type: LlmProvider::Anthropic,
                models: vec!["claude-sonnet-4-20250514".into()],
                api_key: crate::auth::Credential::new(String::new()),
                base_url: None,
            },
        );

        let settings = crate::config::Settings {
            providers,
            ..Default::default()
        };

        let configs = vec![SubagentAgentConfig {
            name: "research_agent".into(),
            description: "Research specialist".into(),
            system_prompt: "You are a researcher.".into(),
            provider: "claude".into(),
            model: None,
            max_iterations: None,
            max_tokens: None,
            tools: None,
        }];

        let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
        let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[]);

        assert_eq!(skill.name(), "subagents");
        assert_eq!(skill.tools().len(), 1);
        assert_eq!(skill.tools()[0].name(), "research_agent");

        let prompt = skill.system_prompt().unwrap();
        assert!(prompt.contains("research_agent"));
        assert!(prompt.contains("Research specialist"));
        assert!(prompt.contains("subagents"));
    }

    #[test]
    fn subagent_skill_skips_unknown_provider() {
        let settings = crate::config::Settings::default(); // no providers

        let configs = vec![SubagentAgentConfig {
            name: "bad_agent".into(),
            description: "Unknown provider".into(),
            system_prompt: "Test".into(),
            provider: "nonexistent".into(),
            model: None,
            max_iterations: None,
            max_tokens: None,
            tools: None,
        }];

        let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
        let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[]);

        // Should have skipped the subagent with unknown provider.
        assert_eq!(skill.tools().len(), 0);
        assert!(skill.system_prompt().is_none());
    }

    // -----------------------------------------------------------------------
    // Tool filtering tests
    // -----------------------------------------------------------------------

    #[test]
    fn filter_tools_none_inherits_all() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(crate::tool::bash::BashTool::default()),
            Arc::new(crate::tool::read_file::ReadFileTool),
        ];
        let filtered = filter_tools(&tools, &None);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn filter_tools_by_name() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(crate::tool::bash::BashTool::default()),
            Arc::new(crate::tool::read_file::ReadFileTool),
            Arc::new(crate::tool::write_file::WriteFileTool),
        ];
        let filtered = filter_tools(&tools, &Some(vec!["bash".into(), "read_file".into()]));
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].name(), "bash");
        assert_eq!(filtered[1].name(), "read_file");
    }

    #[test]
    fn filter_tools_ignores_unknown_names() {
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(crate::tool::bash::BashTool::default())];
        let filtered = filter_tools(&tools, &Some(vec!["bash".into(), "nonexistent".into()]));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name(), "bash");
    }

    #[test]
    fn filter_tools_empty_filter_returns_none() {
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(crate::tool::bash::BashTool::default())];
        let filtered = filter_tools(&tools, &Some(vec![]));
        assert_eq!(filtered.len(), 0);
    }

    // -----------------------------------------------------------------------
    // Built-in subagent tests
    // -----------------------------------------------------------------------

    #[test]
    fn builtin_subagent_configs_returns_planner_and_researcher() {
        let configs = builtin_subagent_configs();
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].name, "planner");
        assert_eq!(configs[1].name, "researcher");
        // Both use the "default" provider.
        assert!(configs.iter().all(|c| c.provider == "default"));
    }

    #[test]
    fn builtin_subagents_have_tool_filters() {
        let configs = builtin_subagent_configs();
        // Planner has read-only tools.
        let planner_tools = configs[0].tools.as_ref().unwrap();
        assert!(planner_tools.contains(&"read_file".to_string()));
        assert!(!planner_tools.contains(&"bash".to_string()));
        // Researcher has broader access.
        let researcher_tools = configs[1].tools.as_ref().unwrap();
        assert!(researcher_tools.contains(&"bash".to_string()));
        assert!(researcher_tools.contains(&"web_search".to_string()));
    }

    #[test]
    fn default_provider_resolves_to_agent_settings() {
        // Create settings with no named providers but a configured agent.
        let settings = crate::config::Settings {
            agent: AgentSettings {
                provider: LlmProvider::Anthropic,
                api_key: crate::auth::Credential::new("test-key".into()),
                base_url: Some("https://custom.api".into()),
                ..Default::default()
            },
            ..Default::default()
        };

        let configs = vec![SubagentAgentConfig {
            name: "test_default".into(),
            description: "Uses default provider".into(),
            system_prompt: "Test".into(),
            provider: "default".into(),
            model: None,
            max_iterations: None,
            max_tokens: None,
            tools: None,
        }];

        let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
        let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[]);

        // Should have resolved successfully (1 tool, not skipped).
        assert_eq!(skill.tools().len(), 1);
        assert_eq!(skill.tools()[0].name(), "test_default");
    }
}
