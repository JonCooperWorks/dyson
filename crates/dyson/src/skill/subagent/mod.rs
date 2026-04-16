// ===========================================================================
// Subagent skill — spawn child agents as tools.
//
// A `SubagentTool` is a Tool that, on invocation, builds a fresh child
// `Agent` with its own LLM client, system prompt, and conversation, runs
// it to completion, and returns the final text as a ToolOutput.
//
//   Parent Agent ──► research_agent (SubagentTool)
//                     └─► Child Agent (runs to completion, returns text)
//
// Invariants:
//   - Shared sandbox: child inherits parent's `Arc<dyn Sandbox>`. Security
//     cannot be bypassed by delegation.
//   - Shared workspace: child sees the same memory files as the parent.
//   - Inherited tools: `Arc<dyn Tool>` clones — no MCP reconnects.
//   - Conversation isolation: only the child's final text reaches the
//     parent; intermediate messages do not.
//   - Recursion cap: subagent tools are excluded from children, with
//     `MAX_SUBAGENT_DEPTH` as a belt-and-suspenders check.
// ===========================================================================

mod coder;
mod orchestrator;
mod security_engineer;

pub use coder::CoderTool;
pub use orchestrator::{OrchestratorConfig, OrchestratorTool};
pub use security_engineer::security_engineer_config;

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

/// An `Output` that accumulates a child agent's streamed text into a
/// buffer.  Tool events are logged at `debug` but never captured —
/// only the final text reaches the parent.
#[derive(Default)]
pub struct CaptureOutput {
    text: String,
}

impl CaptureOutput {
    pub const fn new() -> Self {
        Self { text: String::new() }
    }

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

/// A `Skill` that wraps pre-loaded `Arc<dyn Tool>` pointers for a child
/// agent.  Unlike `BuiltinSkill` or `McpSkill`, it doesn't own tools —
/// it forwards clones from the parent, so there are no lifecycle hooks
/// and MCP connections aren't re-established.
pub struct FilteredSkill {
    tools: Vec<Arc<dyn Tool>>,
}

impl FilteredSkill {
    /// Exposed for integration tests; production callers go through
    /// `SubagentSkill::new` or `CoderTool`, which wrap this internally.
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

/// Arguments for [`spawn_child`].  `SubagentTool` and `CoderTool` each
/// build one of these and delegate the common lifecycle.
pub(crate) struct ChildSpawn<'a> {
    pub name: &'a str,
    pub settings: AgentSettings,
    pub inherited_tools: Vec<Arc<dyn Tool>>,
    pub sandbox: Arc<dyn Sandbox>,
    pub workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,
    pub client: RateLimitedHandle<Box<dyn LlmClient>>,
    /// Depth of the calling parent; the child runs at `parent_depth + 1`.
    pub parent_depth: u8,
    /// Override the child's working directory (used by `CoderTool`).
    pub working_dir: Option<PathBuf>,
    pub user_message: String,
}

/// Build a child `Agent` from `spec`, run it to completion under a
/// `CaptureOutput`, and return its final text as a `ToolOutput`.
///
/// Depth overflow is returned as `ToolOutput::error` (recoverable) rather
/// than `Err`, matching the codebase's split between bad input and
/// runtime failure.
pub(crate) async fn spawn_child(spec: ChildSpawn<'_>) -> Result<ToolOutput> {
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

/// A `Tool` that spawns a child `Agent` per invocation and returns its
/// final text.  Input schema: `{ task: string, context?: string }`.
///
/// The child inherits the parent's sandbox, workspace, and a filtered
/// slice of the parent's tools.  See the module header for invariants.
pub struct SubagentTool {
    config: SubagentAgentConfig,
    /// Resolved provider type for default-model lookup.
    provider: LlmProvider,
    /// Shares the parent's rate-limit window via `ClientRegistry`.
    client: RateLimitedHandle<Box<dyn LlmClient>>,
    sandbox: Arc<dyn Sandbox>,
    workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,
    /// `Arc<dyn Tool>` clones from the parent's already-loaded skills;
    /// MCP connections remain owned by the parent's `McpSkill`.
    inherited_tools: Vec<Arc<dyn Tool>>,
}

impl SubagentTool {
    pub fn new(
        config: SubagentAgentConfig,
        provider: LlmProvider,
        client: RateLimitedHandle<Box<dyn LlmClient>>,
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,
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

/// Parsed input for `SubagentTool`.  Mirrors `input_schema()`.
#[derive(Debug, Deserialize)]
struct SubagentInput {
    task: String,
    #[serde(default)]
    context: String,
}

/// Bundles `SubagentTool` instances (plus the built-in `CoderTool`) into
/// a `Skill` and contributes a system-prompt fragment listing them.
///
/// Built by the parent *after* all other skills load, so it can clone
/// `Arc<dyn Tool>` pointers from their already-initialized tool lists.
pub struct SubagentSkill {
    tools: Vec<Arc<dyn Tool>>,
    system_prompt: String,
}

impl SubagentSkill {
    /// `configs` resolve against `settings.providers`; the sentinel
    /// `"default"` uses the parent's active provider so built-ins work
    /// with no extra config.
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

        // Coder is always present: it takes a `path` and scopes the
        // child's `working_dir`, behavior SubagentTool doesn't expose.
        let (coder_provider, coder_client) =
            (settings.agent.provider.clone(), registry.get_default());
        let coder_tool = CoderTool::new(
            coder_provider.clone(),
            coder_client.clone(),
            Arc::clone(&sandbox),
            workspace.clone(),
            parent_tools,
        );
        prompt_lines.push(format!("- **{}**: {}", coder_tool.name(), coder_tool.description()));
        tools.push(Arc::new(coder_tool));

        // Orchestrators: composable subagents that get direct tools + inner
        // subagent dispatch.  Each is defined by an OrchestratorConfig.
        let orchestrator_configs = builtin_orchestrator_configs();
        if !orchestrator_configs.is_empty() {
            // Build inner subagent tools once — shared across all orchestrators.
            let builtin_configs = builtin_subagent_configs();
            let mut inner_subagent_tools: Vec<Arc<dyn Tool>> = Vec::new();

            for cfg in &builtin_configs {
                let inherited = filter_tools(parent_tools, &cfg.tools);
                let inner_tool = SubagentTool::new(
                    cfg.clone(),
                    settings.agent.provider.clone(),
                    registry.get_default(),
                    Arc::clone(&sandbox),
                    workspace.clone(),
                    inherited,
                );
                inner_subagent_tools.push(Arc::new(inner_tool));
            }

            // Inner coder shared by all orchestrators.
            let inner_coder = CoderTool::new(
                settings.agent.provider.clone(),
                registry.get_default(),
                Arc::clone(&sandbox),
                workspace.clone(),
                parent_tools,
            );
            inner_subagent_tools.push(Arc::new(inner_coder));

            for orch_cfg in orchestrator_configs {
                let orch_tool = OrchestratorTool::new(
                    orch_cfg,
                    coder_provider.clone(),
                    coder_client.clone(),
                    Arc::clone(&sandbox),
                    workspace.clone(),
                    parent_tools,
                    inner_subagent_tools.clone(),
                );
                prompt_lines.push(format!(
                    "- **{}**: {}",
                    orch_tool.name(),
                    orch_tool.description()
                ));
                tools.push(Arc::new(orch_tool));
            }
        }

        // Subagents and orchestrators may contribute usage-protocol
        // fragments to the parent's system prompt.
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

            // Protocol fragments from config-driven subagents.
            for cfg in configs {
                if let Some(ref fragment) = cfg.injects_protocol {
                    prompt.push_str(fragment);
                }
            }

            // Protocol fragments from orchestrators.
            for orch_cfg in builtin_orchestrator_configs() {
                if let Some(ref fragment) = orch_cfg.injects_protocol {
                    prompt.push_str(fragment);
                }
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

/// Built-in subagents that ship with every Dyson instance.  All use the
/// `"default"` provider (the parent's own).  Users can extend or
/// override these in `dyson.json`.
///
/// - `planner`: read-only; produces ordered implementation steps.
/// - `researcher`: broad read access including `bash` and `web_search`.
/// - `verifier`: adversarial validation; injects a usage protocol into
///   the parent's system prompt via `injects_protocol`.
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
            injects_protocol: Some(include_str!("prompts/verifier_protocol.md").into()),
        },
    ]
}

/// Built-in orchestrator configs.  Each is composed into an
/// `OrchestratorTool` with inner subagent dispatch.  Add new orchestrator
/// roles here — they automatically get planner/researcher/coder/verifier
/// as inner subagents.
pub fn builtin_orchestrator_configs() -> Vec<OrchestratorConfig> {
    vec![security_engineer_config()]
}

/// Filter `parent_tools` by the subagent's optional `tools` list.
/// `None` inherits everything; unknown names are silently dropped (use
/// [`filter_tools_checked`] for a warning).
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

/// [`filter_tools`] with a `warn!` for any filter entry that doesn't
/// match a real parent tool, so operators see why a configured tool is
/// missing.
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
