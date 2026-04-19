// ===========================================================================
// OrchestratorTool — a generic composable subagent orchestrator.
//
// An OrchestratorTool spawns a child agent that gets:
//   - A filtered set of parent tools (the "direct tools")
//   - Inner subagent tools (planner, researcher, coder, verifier)
//   - A custom system prompt that defines its personality
//
// Any orchestrator role can be composed from an `OrchestratorConfig`:
//   - security_engineer: AST-aware vuln scanning + exploit generation
//   - (future): architect, devops_engineer, data_engineer, etc.
//
// Architecture:
//   Parent (depth 0)
//     └─ orchestrator child (depth 1)
//          ├─ [direct tools: filtered from parent by allowlist]
//          └─ [inner subagents: planner, researcher, coder, verifier]
//               └─ inner subagent children (depth 2, no further subagents)
// ===========================================================================

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::agent::rate_limiter::RateLimitedHandle;
use crate::config::{AgentSettings, LlmProvider};
use crate::error::{DysonError, Result};
use crate::llm::LlmClient;
use crate::sandbox::Sandbox;
use crate::tool::{Tool, ToolContext, ToolOutput};
use crate::workspace::WorkspaceHandle;

use super::{ChildSpawn, spawn_child};

/// Configuration that defines an orchestrator's identity and capabilities.
/// Compose any orchestrator role by filling in this struct.
///
/// Uses `&'static str` for fields that are typically `include_str!()` or
/// string literals (system_prompt, injects_protocol, tool names) to avoid
/// unnecessary heap allocations at startup.
#[derive(Clone, Debug)]
pub struct OrchestratorConfig {
    /// Tool name exposed to the parent agent (e.g., "security_engineer").
    pub name: &'static str,
    /// Tool description shown in the parent's tool list.
    pub description: &'static str,
    /// System prompt for the orchestrator's child agent.
    pub system_prompt: &'static str,
    /// Allowlist of parent tool names this orchestrator gets direct access to.
    /// Tools not in this list are filtered out.
    pub direct_tool_names: &'static [&'static str],
    /// Max iterations for the child agent.
    pub max_iterations: usize,
    /// Max tokens per LLM response.
    pub max_tokens: u32,
    /// Optional protocol fragment injected into the parent's system prompt.
    /// Tells the parent when and how to invoke this orchestrator.
    pub injects_protocol: Option<&'static str>,
    /// When true, detect languages/frameworks in the scoped review
    /// directory at call time and append matching cheatsheets to the
    /// child's system prompt.  Only `security_engineer` sets this —
    /// other orchestrators (future devops, architect, ...) don't want
    /// vuln cheatsheets.  Detection cost: one shallow directory walk
    /// and 1–3 `toml` / `json` parses per invocation.
    pub inject_cheatsheets: bool,
}

/// A composable `Tool` that spawns an orchestrator child agent.
///
/// The child gets direct tools (filtered from parent) plus inner subagent
/// tools (planner, researcher, coder, verifier) for parallel dispatch.
///
/// Input schema: `{ task: string, context?: string }`
pub struct OrchestratorTool {
    config: OrchestratorConfig,
    provider: LlmProvider,
    /// Model identifier inherited from the parent agent's configuration.
    /// Never falls back to a registry default — subagents must bill the
    /// same model the user configured, not a hardcoded Sonnet.
    model: String,
    client: RateLimitedHandle<Box<dyn LlmClient>>,
    sandbox: Arc<dyn Sandbox>,
    workspace: Option<WorkspaceHandle>,
    /// Parent tools filtered to `config.direct_tool_names`.
    pub(crate) direct_tools: Vec<Arc<dyn Tool>>,
    /// Inner subagent tools (planner, researcher, coder, verifier) that
    /// the child can dispatch at depth 2.
    pub(crate) inner_subagent_tools: Vec<Arc<dyn Tool>>,
}

impl OrchestratorTool {
    /// Create a new OrchestratorTool from a config.
    ///
    /// `parent_tools` is filtered to `config.direct_tool_names`.
    /// `inner_subagent_tools` are pre-built SubagentTool/CoderTool instances
    /// that the child can invoke (they spawn at depth 2).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: OrchestratorConfig,
        provider: LlmProvider,
        model: String,
        client: RateLimitedHandle<Box<dyn LlmClient>>,
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<WorkspaceHandle>,
        parent_tools: &[Arc<dyn Tool>],
        inner_subagent_tools: Vec<Arc<dyn Tool>>,
    ) -> Self {
        let direct_tools = parent_tools
            .iter()
            .filter(|t| config.direct_tool_names.contains(&t.name()))
            .cloned()
            .collect();

        Self {
            config,
            provider,
            model,
            client,
            sandbox,
            workspace,
            direct_tools,
            inner_subagent_tools,
        }
    }

    /// Access the underlying config.
    pub fn config(&self) -> &OrchestratorConfig {
        &self.config
    }
}

#[async_trait]
impl Tool for OrchestratorTool {
    fn name(&self) -> &str {
        self.config.name
    }

    fn description(&self) -> &str {
        self.config.description
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task for this orchestrator to perform."
                },
                "context": {
                    "type": "string",
                    "description": "Optional background context about the codebase, \
                        recent changes, or specific areas of concern."
                },
                "path": {
                    "type": "string",
                    "description": "Optional directory to scope the orchestrator's \
                        child agent to.  When set, the child's working directory is \
                        this path — relative tool paths resolve against it and `bash` \
                        starts here.  Falls back to the parent's working directory \
                        when omitted."
                }
            },
            "required": ["task"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let parsed: OrchestratorInput =
            serde_json::from_value(input.clone()).map_err(|e| {
                DysonError::tool(self.config.name, format!("invalid input: {e}"))
            })?;

        // Validate + canonicalize the optional scope path before handing it
        // to the child.  `canonicalize` also implicitly checks existence.
        let scoped_dir = if parsed.path.is_empty() {
            None
        } else {
            let requested = std::path::PathBuf::from(&parsed.path);
            let resolved = if requested.is_absolute() {
                requested
            } else {
                ctx.working_dir.join(&requested)
            };
            let canonical = match resolved.canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolOutput::error(format!(
                        "path '{}' cannot be resolved: {e}",
                        parsed.path
                    )));
                }
            };
            if !canonical.is_dir() {
                return Ok(ToolOutput::error(format!(
                    "path '{}' is not a directory",
                    parsed.path
                )));
            }
            Some(canonical)
        };

        let user_message = if parsed.context.is_empty() {
            parsed.task
        } else {
            format!("Context:\n{}\n\nTask:\n{}", parsed.context, parsed.task)
        };

        // Combine direct + inner subagent tools without cloning the Vec.
        let mut all_tools =
            Vec::with_capacity(self.direct_tools.len() + self.inner_subagent_tools.len());
        all_tools.extend(self.direct_tools.iter().cloned());
        all_tools.extend(self.inner_subagent_tools.iter().cloned());

        // Compose the child's system prompt.  Cheatsheets attach only
        // for orchestrators that opt in (security_engineer today).
        // Detection runs against the effective review root — the
        // scoped `path` if provided, else the parent's working dir.
        let mut system_prompt = self.config.system_prompt.to_string();
        if self.config.inject_cheatsheets && cheatsheets_enabled_via_env() {
            let detect_root: &std::path::Path = scoped_dir
                .as_deref()
                .unwrap_or(ctx.working_dir.as_path());
            let (body, sheets) =
                super::repo_detect::detect_and_compose(detect_root);
            if !sheets.is_empty() {
                tracing::info!(
                    tool = self.config.name,
                    sheets = ?sheets,
                    "cheatsheets injected into security_engineer system prompt"
                );
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&body);
            } else {
                tracing::info!(
                    tool = self.config.name,
                    "no cheatsheets matched — injecting none"
                );
            }
        }

        let settings = AgentSettings {
            model: self.model.clone(),
            max_iterations: self.config.max_iterations,
            max_tokens: self.config.max_tokens,
            system_prompt,
            provider: self.provider.clone(),
            ..AgentSettings::default()
        };

        spawn_child(ChildSpawn {
            name: self.config.name,
            settings,
            inherited_tools: all_tools,
            sandbox: Arc::clone(&self.sandbox),
            workspace: self.workspace.clone(),
            client: self.client.clone(),
            parent_depth: ctx.depth,
            working_dir: scoped_dir,
            user_message,
        })
        .await
    }
}

/// Environment-level kill switch for cheatsheet injection.  The
/// `expensive_live_security_review` example sets this from its
/// `--cheatsheets {on,off}` flag so A/B runs against the same target
/// can measure the effect of the sheets.  Values that read as "off":
/// `off`, `false`, `0`, `no`.  Anything else (including unset) = on.
///
/// Env-var gating keeps the example from having to rebuild or mutate
/// the baked-in `OrchestratorConfig` — the tool is handed back through
/// `create_skills` as an `Arc<dyn Tool>` with no outward config handle.
fn cheatsheets_enabled_via_env() -> bool {
    match std::env::var("DYSON_SECURITY_ENGINEER_CHEATSHEETS") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "false" | "0" | "no"
        ),
        Err(_) => true,
    }
}

/// Parsed input for `OrchestratorTool`.
#[derive(Debug, Deserialize)]
struct OrchestratorInput {
    task: String,
    #[serde(default)]
    context: String,
    /// Optional directory to scope the child agent to.  See `input_schema`.
    #[serde(default)]
    path: String,
}
