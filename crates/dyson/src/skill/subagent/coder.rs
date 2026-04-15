// ===========================================================================
// CoderTool — a subagent scoped to a specific directory for coding tasks.
//
// Unlike the general SubagentTool (which takes task + context and operates
// in the parent's working directory), CoderTool takes path + task, resolves
// the path, and scopes the child agent's working_dir to that directory.
//
// The child agent gets a constrained toolset (bash, read_file, edit_file,
// search_files, list_files, bulk_edit) and a coding-focused system prompt.
// Both tools share the lifecycle helper `super::spawn_child`.
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

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Tools available to the coder subagent.
const CODER_TOOLS: &[&str] = &[
    "bash",
    "read_file",
    "edit_file",
    "search_files",
    "list_files",
    "bulk_edit",
];

/// System prompt for the coder subagent.
const CODER_SYSTEM_PROMPT: &str = include_str!("prompts/coder.md");

// ---------------------------------------------------------------------------
// CoderTool
// ---------------------------------------------------------------------------

/// A tool that spawns a coding subagent scoped to a specific directory.
///
/// The parent LLM calls this to delegate focused coding work:
///
/// ```json
/// {
///   "name": "coder",
///   "input": {
///     "path": "crates/auth",
///     "task": "Rename the Config struct to AuthConfig across all files."
///   }
/// }
/// ```
///
/// The child agent runs with a constrained toolset and returns a summary
/// of the changes it made.
pub struct CoderTool {
    /// Resolved provider type (for default model lookup).
    provider: LlmProvider,

    /// Shared LLM client handle — from the same `ClientRegistry` as the
    /// parent agent.
    client: crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>,

    /// Shared sandbox — same instance as the parent agent.
    sandbox: Arc<dyn Sandbox>,

    /// Shared workspace — same instance as the parent agent.
    workspace: Option<Arc<RwLock<Box<dyn crate::workspace::Workspace>>>>,

    /// Tools inherited from the parent, filtered to CODER_TOOLS.
    pub(crate) inherited_tools: Vec<Arc<dyn Tool>>,
}

impl CoderTool {
    /// Construct a new CoderTool.
    ///
    /// Filters `parent_tools` down to the coding-relevant subset defined
    /// by [`CODER_TOOLS`].
    pub fn new(
        provider: LlmProvider,
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

        let scoped_dir = crate::tool::resolve_and_validate_path(&ctx.working_dir, &parsed.path)
            .map_err(|e| DysonError::tool("coder", e))?;

        if !scoped_dir.is_dir() {
            return Ok(ToolOutput::error(format!(
                "path '{}' is not a directory",
                scoped_dir.display()
            )));
        }

        let settings = AgentSettings {
            model: crate::llm::registry::lookup(&self.provider)
                .default_model
                .to_string(),
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

/// Parsed input payload for `CoderTool`.
///
/// Both `path` and `task` are required.  Using serde lets the caller
/// get field-level error messages (e.g., `missing field 'task'`) for
/// free, matching the `bash.rs` idiom of `DysonError::tool(name, ...)`
/// for malformed tool inputs.
#[derive(Debug, Deserialize)]
struct CoderInput {
    path: String,
    task: String,
}
