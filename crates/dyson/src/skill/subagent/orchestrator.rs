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
use tokio::sync::RwLock;

use crate::agent::rate_limiter::RateLimitedHandle;
use crate::config::{AgentSettings, LlmProvider};
use crate::error::{DysonError, Result};
use crate::llm::LlmClient;
use crate::sandbox::Sandbox;
use crate::tool::{Tool, ToolContext, ToolOutput};
use crate::workspace::Workspace;

use super::{ChildSpawn, spawn_child};

/// Configuration that defines an orchestrator's identity and capabilities.
/// Compose any orchestrator role by filling in this struct.
#[derive(Clone, Debug)]
pub struct OrchestratorConfig {
    /// Tool name exposed to the parent agent (e.g., "security_engineer").
    pub name: String,
    /// Tool description shown in the parent's tool list.
    pub description: String,
    /// System prompt for the orchestrator's child agent.
    pub system_prompt: String,
    /// Allowlist of parent tool names this orchestrator gets direct access to.
    /// Tools not in this list are filtered out.
    pub direct_tool_names: Vec<String>,
    /// Max iterations for the child agent.
    pub max_iterations: usize,
    /// Max tokens per LLM response.
    pub max_tokens: u32,
    /// Optional protocol fragment injected into the parent's system prompt.
    /// Tells the parent when and how to invoke this orchestrator.
    pub injects_protocol: Option<String>,
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
    client: RateLimitedHandle<Box<dyn LlmClient>>,
    sandbox: Arc<dyn Sandbox>,
    workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,
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
    pub fn new(
        config: OrchestratorConfig,
        provider: LlmProvider,
        client: RateLimitedHandle<Box<dyn LlmClient>>,
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,
        parent_tools: &[Arc<dyn Tool>],
        inner_subagent_tools: Vec<Arc<dyn Tool>>,
    ) -> Self {
        let direct_tools = parent_tools
            .iter()
            .filter(|t| config.direct_tool_names.iter().any(|n| n == t.name()))
            .cloned()
            .collect();

        Self {
            config,
            provider,
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
                    "description": "The task for this orchestrator to perform."
                },
                "context": {
                    "type": "string",
                    "description": "Optional background context about the codebase, \
                        recent changes, or specific areas of concern."
                }
            },
            "required": ["task"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let parsed: OrchestratorInput =
            serde_json::from_value(input.clone()).map_err(|e| {
                DysonError::tool(&self.config.name, format!("invalid input: {e}"))
            })?;

        let user_message = if parsed.context.is_empty() {
            parsed.task
        } else {
            format!(
                "Context:\n{}\n\nTask:\n{}",
                parsed.context, parsed.task
            )
        };

        // Combine direct tools + inner subagent tools into one flat list.
        let mut all_tools = self.direct_tools.clone();
        all_tools.extend(self.inner_subagent_tools.iter().cloned());

        let settings = AgentSettings {
            model: crate::llm::registry::lookup(&self.provider)
                .default_model
                .to_string(),
            max_iterations: self.config.max_iterations,
            max_tokens: self.config.max_tokens,
            system_prompt: self.config.system_prompt.clone(),
            provider: self.provider.clone(),
            ..AgentSettings::default()
        };

        spawn_child(ChildSpawn {
            name: &self.config.name,
            settings,
            inherited_tools: all_tools,
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

/// Parsed input for `OrchestratorTool`.
#[derive(Debug, Deserialize)]
struct OrchestratorInput {
    task: String,
    #[serde(default)]
    context: String,
}
