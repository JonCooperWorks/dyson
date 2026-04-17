// ===========================================================================
// CoderTool — a subagent scoped to a specific directory for coding tasks.
//
// Input is `{ path, task }`.  The path is resolved against the parent's
// working directory and becomes the child's `working_dir`.  The child
// runs with a constrained toolset (`CODER_TOOLS`) and the coder prompt.
// Shares the lifecycle helper in `super::spawn_child`.
// ===========================================================================

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::config::{AgentSettings, LlmProvider};
use crate::error::{DysonError, Result};
use crate::sandbox::Sandbox;
use crate::tool::{Tool, ToolContext, ToolOutput};

use super::{ChildSpawn, spawn_child};

const CODER_TOOLS: &[&str] = &[
    "bash",
    "read_file",
    "edit_file",
    "search_files",
    "list_files",
    "bulk_edit",
];

const CODER_SYSTEM_PROMPT: &str = include_str!("prompts/coder.md");

/// A `Tool` that delegates a coding task to a child agent scoped to one
/// directory.
///
/// ```json
/// { "name": "coder",
///   "input": { "path": "crates/auth",
///              "task": "Rename Config to AuthConfig across all files." } }
/// ```
pub struct CoderTool {
    provider: LlmProvider,
    /// Model identifier inherited from the parent agent's configuration.
    /// Never falls back to a registry default — subagents must bill the
    /// same model the user configured, not a hardcoded Sonnet.
    model: String,
    client: crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>,
    sandbox: Arc<dyn Sandbox>,
    workspace: Option<Arc<RwLock<Box<dyn crate::workspace::Workspace>>>>,
    /// Parent tools filtered to `CODER_TOOLS`.
    pub(crate) inherited_tools: Vec<Arc<dyn Tool>>,
}

impl CoderTool {
    /// Filters `parent_tools` down to [`CODER_TOOLS`].
    pub fn new(
        provider: LlmProvider,
        model: String,
        client: crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>,
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<Arc<RwLock<Box<dyn crate::workspace::Workspace>>>>,
        parent_tools: &[Arc<dyn Tool>],
    ) -> Self {
        let inherited_tools = parent_tools
            .iter()
            .filter(|t| CODER_TOOLS.contains(&t.name()))
            .cloned()
            .collect();

        Self {
            provider,
            model,
            client,
            sandbox,
            workspace,
            inherited_tools,
        }
    }
}

#[async_trait]
impl Tool for CoderTool {
    fn name(&self) -> &str {
        "coder"
    }

    fn description(&self) -> &str {
        "Spawns a focused coding subagent scoped to a specific directory.  \
         Give it a path and a task, and it will read, edit, and verify code \
         within that directory.  Returns a summary of changes made.  Use for \
         well-defined coding tasks that can be completed independently."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to scope the coder to (relative to working directory)."
                },
                "task": {
                    "type": "string",
                    "description": "Natural language description of the coding task to complete."
                }
            },
            "required": ["path", "task"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let parsed: CoderInput = serde_json::from_value(input.clone())
            .map_err(|e| DysonError::tool("coder", format!("invalid input: {e}")))?;

        let scoped_dir = match ctx.resolve_path(&parsed.path) { Ok(p) => p, Err(e) => return Ok(e) };

        if !scoped_dir.is_dir() {
            return Ok(ToolOutput::error(format!(
                "path '{}' is not a directory",
                scoped_dir.display()
            )));
        }

        let settings = AgentSettings {
            model: self.model.clone(),
            max_iterations: 30,
            max_tokens: 8192,
            system_prompt: CODER_SYSTEM_PROMPT.to_string(),
            provider: self.provider.clone(),
            ..AgentSettings::default()
        };

        let user_message = format!(
            "Your working directory has been set to the scoped path.  \
             Complete the following task:\n\n{}",
            parsed.task,
        );

        spawn_child(ChildSpawn {
            name: "coder",
            settings,
            inherited_tools: self.inherited_tools.clone(),
            sandbox: Arc::clone(&self.sandbox),
            workspace: self.workspace.clone(),
            client: self.client.clone(),
            parent_depth: ctx.depth,
            working_dir: Some(scoped_dir),
            user_message,
        })
        .await
    }
}

/// Parsed input for `CoderTool`.  Mirrors `input_schema()`.
#[derive(Debug, Deserialize)]
struct CoderInput {
    path: String,
    task: String,
}
