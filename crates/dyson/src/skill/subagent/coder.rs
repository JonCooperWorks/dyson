// ===========================================================================
// CoderTool — a subagent scoped to a specific directory for coding tasks.
//
// Unlike the general SubagentTool (which takes task + context and operates
// in the parent's working directory), CoderTool takes path + task, resolves
// the path, and scopes the child agent's working_dir to that directory.
//
// The child agent gets a constrained toolset (bash, read_file, edit_file,
// search_files, list_files, bulk_edit) and a coding-focused system prompt.
// ===========================================================================

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::config::{AgentSettings, LlmProvider};
use crate::error::{DysonError, Result};
use crate::sandbox::Sandbox;
use crate::skill::Skill;
use crate::tool::{MAX_SUBAGENT_DEPTH, Tool, ToolContext, ToolOutput};

use super::{CaptureOutput, FilteredSkill};

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
const CODER_SYSTEM_PROMPT: &str = "\
You are a focused code editor working within a specific directory.  \
Your job is to complete the coding task described below using only \
the tools available to you.\n\n\
Rules:\n\
1. Only modify files within your scoped directory.\n\
2. Read files before editing to understand context.\n\
3. Use bulk_edit for structural refactors and multi-file find/replace, edit_file for targeted changes.\n\
4. After making changes, verify them (search for old references, read modified files).\n\
5. Report a concise summary of what you changed when done.";

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
        // -- Check recursion depth --
        if ctx.depth >= MAX_SUBAGENT_DEPTH {
            return Ok(ToolOutput::error(format!(
                "Maximum subagent nesting depth ({MAX_SUBAGENT_DEPTH}) reached. \
                 Cannot spawn coder subagent."
            )));
        }

        // -- Extract inputs --
        let path_str = input["path"]
            .as_str()
            .ok_or_else(|| DysonError::tool("coder", "missing required 'path' field"))?;

        let task = input["task"]
            .as_str()
            .ok_or_else(|| DysonError::tool("coder", "missing required 'task' field"))?;

        // -- Resolve and validate path --
        let scoped_dir = crate::tool::resolve_and_validate_path(&ctx.working_dir, path_str)
            .map_err(|e| DysonError::tool("coder", e))?;

        // Ensure the resolved path is a directory.
        if !scoped_dir.is_dir() {
            return Ok(ToolOutput::error(format!(
                "path '{}' is not a directory",
                scoped_dir.display()
            )));
        }

        tracing::info!(
            tool = "coder",
            depth = ctx.depth + 1,
            path = %scoped_dir.display(),
            "spawning coder subagent"
        );

        // -- Build child agent settings --
        let model = crate::llm::registry::lookup(&self.provider)
            .default_model
            .to_string();

        let child_settings = AgentSettings {
            model,
            max_iterations: 30,
            max_tokens: 8192,
            system_prompt: CODER_SYSTEM_PROMPT.to_string(),
            provider: self.provider.clone(),
            ..AgentSettings::default()
        };

        // -- Build skills from inherited tools --
        let skills: Vec<Box<dyn Skill>> = vec![Box::new(FilteredSkill {
            tools: self.inherited_tools.clone(),
        })];

        // -- Create the child agent --
        let mut builder = crate::agent::Agent::builder(self.client.clone(), Arc::clone(&self.sandbox))
            .skills(skills)
            .settings(&child_settings);
        if let Some(ws) = &self.workspace {
            builder = builder.workspace(Arc::clone(ws));
        }
        let mut child_agent = builder.build()?;

        // Scope the child to the target directory and increment depth.
        child_agent.set_working_dir(scoped_dir);
        child_agent.set_depth(ctx.depth + 1);

        // -- Run with captured output --
        let user_message = format!(
            "Your working directory has been set to the scoped path.  \
             Complete the following task:\n\n{task}"
        );

        let mut capture = CaptureOutput::new();
        match child_agent.run(&user_message, &mut capture).await {
            Ok(final_text) => {
                tracing::info!(
                    tool = "coder",
                    result_len = final_text.len(),
                    "coder subagent completed successfully"
                );
                Ok(ToolOutput::success(final_text))
            }
            Err(e) => {
                tracing::warn!(
                    tool = "coder",
                    error = %e,
                    "coder subagent failed"
                );
                Ok(ToolOutput::error(format!("Coder subagent failed: {e}")))
            }
        }
    }
}
