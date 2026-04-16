// ===========================================================================
// SecurityEngineerTool — an orchestrator subagent for security analysis.
//
// Like CoderTool, this is a custom `Tool` that spawns a child agent.
// Unlike CoderTool, the child gets:
//   - Security-specific tools (ast_query, attack_surface_analyzer, exploit_builder)
//   - Inner subagent tools (planner, researcher, coder, verifier)
//     so it can dispatch parallel work at depth 2.
//
// Architecture:
//   Parent (depth 0)
//     └─ security_engineer child (depth 1)
//          ├─ [direct tools: bash, read_file, search_files, list_files,
//          │                  ast_query, attack_surface_analyzer, exploit_builder]
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

/// Tools the security_engineer child agent gets direct access to.
const SECURITY_DIRECT_TOOLS: &[&str] = &[
    "bash",
    "read_file",
    "search_files",
    "list_files",
    "ast_query",
    "attack_surface_analyzer",
    "exploit_builder",
];

const SECURITY_ENGINEER_SYSTEM_PROMPT: &str =
    include_str!("prompts/security_engineer.md");

/// A `Tool` that spawns a security_engineer child agent with AST-aware
/// security tools and inner subagent dispatch capability.
///
/// ```json
/// { "name": "security_engineer",
///   "input": { "task": "Review the auth module for vulnerabilities",
///              "context": "We added OAuth2 in src/auth/" } }
/// ```
pub struct SecurityEngineerTool {
    provider: LlmProvider,
    client: RateLimitedHandle<Box<dyn LlmClient>>,
    sandbox: Arc<dyn Sandbox>,
    workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,
    /// Security + general tools filtered from the parent.
    pub(crate) direct_tools: Vec<Arc<dyn Tool>>,
    /// Inner subagent tools (planner, researcher, coder, verifier) that
    /// the child can dispatch at depth 2.
    pub(crate) inner_subagent_tools: Vec<Arc<dyn Tool>>,
}

impl SecurityEngineerTool {
    /// Create a new SecurityEngineerTool.
    ///
    /// `parent_tools` is filtered to [`SECURITY_DIRECT_TOOLS`].
    /// `inner_subagent_tools` are pre-built SubagentTool/CoderTool instances
    /// that the child can invoke (they spawn at depth 2).
    pub fn new(
        provider: LlmProvider,
        client: RateLimitedHandle<Box<dyn LlmClient>>,
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,
        parent_tools: &[Arc<dyn Tool>],
        inner_subagent_tools: Vec<Arc<dyn Tool>>,
    ) -> Self {
        let direct_tools = parent_tools
            .iter()
            .filter(|t| SECURITY_DIRECT_TOOLS.contains(&t.name()))
            .cloned()
            .collect();

        Self {
            provider,
            client,
            sandbox,
            workspace,
            direct_tools,
            inner_subagent_tools,
        }
    }
}

#[async_trait]
impl Tool for SecurityEngineerTool {
    fn name(&self) -> &str {
        "security_engineer"
    }

    fn description(&self) -> &str {
        "Spawns a security engineer agent that performs comprehensive security \
         analysis using AST-aware tools.  Can write custom tree-sitter queries \
         to trace vulnerability patterns, map attack surfaces, generate exploit \
         PoCs, and dispatch subagents (researcher, coder, verifier) in parallel.  \
         Use for security reviews, vulnerability assessments, and validating \
         security-sensitive changes."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The security analysis task to perform."
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
        let parsed: SecurityEngineerInput =
            serde_json::from_value(input.clone()).map_err(|e| {
                DysonError::tool("security_engineer", format!("invalid input: {e}"))
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
            max_iterations: 40,
            max_tokens: 8192,
            system_prompt: SECURITY_ENGINEER_SYSTEM_PROMPT.to_string(),
            provider: self.provider.clone(),
            ..AgentSettings::default()
        };

        spawn_child(ChildSpawn {
            name: "security_engineer",
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

/// Parsed input for `SecurityEngineerTool`.
#[derive(Debug, Deserialize)]
struct SecurityEngineerInput {
    task: String,
    #[serde(default)]
    context: String,
}
