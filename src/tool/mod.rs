// ===========================================================================
// Tool trait — the fundamental unit of capability in Dyson.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Defines the `Tool` trait that every callable capability implements,
//   plus the supporting types `ToolContext` and `ToolOutput`.  Whether it's
//   a bash shell, a file reader, or an MCP remote tool — they all implement
//   this one trait.
//
// Module layout:
//   mod.rs              — Tool trait, ToolContext, ToolOutput (this file)
//   bash.rs             — Shell execution tool
//   workspace_view.rs   — View/list workspace files
//   workspace_search.rs — Search across workspace files
//   workspace_update.rs — Update workspace files (set/append)
//
// How tools fit into the architecture:
//
//   Skill (owns tools)
//     │
//     ├── Arc<dyn Tool>  ─── BashTool
//     ├── Arc<dyn Tool>  ─── WorkspaceViewTool
//     ├── Arc<dyn Tool>  ─── WorkspaceSearchTool
//     ├── Arc<dyn Tool>  ─── WorkspaceUpdateTool
//     └── Arc<dyn Tool>  ─── McpRemoteTool     (MCP servers)
//           │
//           ▼
//   Agent (flat lookup: HashMap<name, Arc<dyn Tool>>)
//     │
//     ▼
//   Sandbox.check(name, input, ctx)  ← gates every call
//     │
//     ▼
//   tool.run(input, ctx)  → ToolOutput
//     │
//     ▼
//   Sandbox.after(name, input, &mut output)  ← post-processing
//
// Why Arc<dyn Tool>?
//   Tools are *owned* by skills but *looked up* by the agent's flat
//   HashMap.  We need shared ownership without lifetime gymnastics.
//   Arc<dyn Tool> is the natural choice: tools are created once and
//   never mutated, so the Arc overhead is negligible (no contention).
//
// Why async?
//   Tools do I/O: bash spawns processes, MCP tools make network calls,
//   file tools hit the filesystem.  Making `run()` async means the
//   tokio runtime can multiplex tool execution efficiently.  Even for
//   fast tools (read a small file), the overhead of an async call is
//   trivial compared to the I/O itself.
// ===========================================================================

pub mod bash;
pub mod edit_file;
pub mod list_files;
pub mod memory_search;
pub mod read_file;
pub mod search_files;
pub mod send_file;
pub mod web_search;
pub mod workspace_search;
pub mod workspace_update;
pub mod workspace_view;
pub mod write_file;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::error::Result;

// ---------------------------------------------------------------------------
// Tool trait
// ---------------------------------------------------------------------------

/// A single callable capability — the fundamental building block of Dyson.
///
/// Every tool has a name, a description (shown to the LLM so it knows when
/// to use it), a JSON schema for its input, and an async `run` method.
///
/// ## Object safety
///
/// This trait is object-safe thanks to `async_trait` (which boxes the
/// returned future).  Tools are stored as `Arc<dyn Tool>` throughout Dyson.
///
/// ## Implementing a new tool
///
/// ```ignore
/// struct MyTool;
///
/// #[async_trait]
/// impl Tool for MyTool {
///     fn name(&self) -> &str { "my_tool" }
///     fn description(&self) -> &str { "Does something useful" }
///     fn input_schema(&self) -> serde_json::Value {
///         serde_json::json!({
///             "type": "object",
///             "properties": {
///                 "input": { "type": "string" }
///             },
///             "required": ["input"]
///         })
///     }
///     async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
///         let val = input["input"].as_str().unwrap_or("default");
///         Ok(ToolOutput::success(format!("Got: {val}")))
///     }
/// }
/// ```
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's unique name, used for dispatch and display.
    ///
    /// Must be a valid identifier (lowercase, underscores) — it appears in
    /// the LLM's tool_use blocks and in log output.
    fn name(&self) -> &str;

    /// Human-readable description shown to the LLM.
    ///
    /// The LLM uses this to decide *when* to call the tool.  Be specific
    /// about what the tool does and when it's appropriate.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's input parameters.
    ///
    /// Sent to the LLM as part of the tool definition so it knows what
    /// arguments to provide.  Must be a valid JSON Schema object.
    fn input_schema(&self) -> serde_json::Value;

    /// Whether this tool should only be available when Dyson executes
    /// tools directly (ToolMode::Execute).
    ///
    /// When `true`, the tool is excluded from the prompt sent to providers
    /// that handle tools internally (Claude Code, Codex) since they already
    /// have equivalent built-in capabilities.
    ///
    /// Defaults to `false` (tool is available to all providers).
    fn agent_only(&self) -> bool {
        false
    }

    /// Execute the tool with the given input and context.
    ///
    /// `input` is the JSON object the LLM provided (validated against
    /// `input_schema` by the LLM, but always validate defensively).
    /// `ctx` provides the working directory, environment, and cancellation
    /// token.
    ///
    /// Returns `ToolOutput` on success (which may still indicate a
    /// "tool-level error" via `is_error`), or `DysonError` for
    /// infrastructure failures.
    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput>;
}

// ---------------------------------------------------------------------------
// ToolContext — ambient state available to every tool call.
// ---------------------------------------------------------------------------

/// Maximum nesting depth for subagent spawning.
///
/// Prevents infinite recursion when subagents spawn subagents.  A top-level
/// agent runs at depth 0; each subagent increments by 1.  When depth reaches
/// this limit, the subagent tool returns an error instead of spawning.
pub const MAX_SUBAGENT_DEPTH: u8 = 3;

/// Runtime context passed to every tool invocation.
///
/// Tools should use this instead of querying the environment directly.
/// This makes tools testable (inject a fake working dir, mock env) and
/// ensures consistent behavior across the agent session.
pub struct ToolContext {
    /// The working directory for this agent session.
    ///
    /// Bash commands run here.  File paths are resolved relative to this.
    /// Set once at startup from the process CWD.
    pub working_dir: PathBuf,

    /// Environment variables available to child processes.
    ///
    /// Tools like bash pass these to spawned commands.  Populated from
    /// the agent config and any skill-specific env vars.
    pub env: HashMap<String, String>,

    /// Cancellation token for cooperative cancellation.
    ///
    /// Long-running tools (bash commands, MCP calls) should poll this
    /// token and abort promptly when it fires.  Triggered by Ctrl-C
    /// in the terminal.
    pub cancellation: CancellationToken,

    /// Workspace for agent identity/memory operations.
    ///
    /// Shared via `Arc<RwLock>` because multiple tools need concurrent
    /// access — reads (view, search) can proceed in parallel, writes
    /// (update, journal) get exclusive access.
    ///
    /// `None` when workspace is not configured (e.g., tests without
    /// workspace setup, or when the provider handles tools internally).
    pub workspace: Option<Arc<RwLock<Box<dyn crate::workspace::Workspace>>>>,

    /// Subagent nesting depth.  0 = top-level agent.
    ///
    /// Used to prevent infinite recursion when subagents spawn subagents.
    /// The subagent tool checks this before spawning a child agent and
    /// returns an error if `depth >= MAX_SUBAGENT_DEPTH`.
    ///
    /// Flows through `ToolContext` (not `AgentSettings`) because depth is
    /// runtime state, not configuration — you don't set it in dyson.json.
    pub depth: u8,
}

impl ToolContext {
    /// Create a context with the current working directory and no extra env.
    ///
    /// Useful for testing and simple setups where you don't need custom
    /// environment variables.
    pub fn from_cwd() -> Result<Self> {
        Ok(Self {
            working_dir: std::env::current_dir()?,
            env: HashMap::new(),
            cancellation: CancellationToken::new(),
            workspace: None,
            depth: 0,
        })
    }
}

// ---------------------------------------------------------------------------
// ToolOutput — what a tool returns to the agent.
// ---------------------------------------------------------------------------

/// The result of a tool execution, sent back to the LLM.
///
/// `content` is the main output text.  `is_error` indicates whether the
/// tool itself considers this an error (not to be confused with
/// `DysonError`, which means the tool *couldn't run at all*).
///
/// Example: a bash command that exits with code 1 returns
/// `ToolOutput { content: "command not found", is_error: true }`.
/// A bash command that exits 0 returns `is_error: false`.
/// A bash command that can't even be spawned returns `Err(DysonError::Io(...))`.
pub struct ToolOutput {
    /// The text content to send back to the LLM.
    pub content: String,

    /// Whether this is a tool-level error.
    ///
    /// The LLM sees this flag in the `tool_result` content block and can
    /// decide to retry, try a different approach, or report the error.
    pub is_error: bool,

    /// Optional structured metadata (not sent to the LLM).
    ///
    /// Used for internal tracking: timing info, exit codes, byte counts,
    /// etc.  Skills can inspect this in their `after_tool()` hook.
    pub metadata: Option<serde_json::Value>,

    /// Files to send to the user via the controller.
    ///
    /// Tools can attach file paths here and the controller's `Output`
    /// implementation will deliver them (e.g., Telegram sends them as
    /// documents, the terminal prints the path).  The files are sent
    /// *after* the text content — they are not included in the LLM's
    /// conversation history.
    pub files: Vec<PathBuf>,
}

/// Validate a workspace file path to prevent path traversal.
///
/// Rejects absolute paths, parent-directory components (`..`), and
/// paths containing symlink components that could escape the workspace.
pub fn validate_workspace_path(path: &str) -> std::result::Result<(), String> {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return Err(format!("absolute paths are not allowed: '{path}'"));
    }
    if p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(format!("path traversal is not allowed: '{path}'"));
    }
    // Check each prefix for symlinks that could escape the workspace.
    let mut accumulated = std::path::PathBuf::new();
    for component in p.components() {
        accumulated.push(component);
        if accumulated.is_symlink() {
            return Err(format!(
                "symlinks are not allowed in workspace paths: '{}' is a symlink",
                accumulated.display()
            ));
        }
    }
    Ok(())
}

/// Resolve a user-provided path relative to the working directory and
/// verify it does not escape the working directory boundary.
///
/// Accepts both relative and absolute paths.  For existing files, the path
/// is canonicalized (resolving symlinks).  For new files (e.g., write_file),
/// the nearest existing ancestor is canonicalized.
///
/// Returns the resolved absolute path on success, or a human-readable
/// error string on failure.
pub fn resolve_and_validate_path(
    working_dir: &std::path::Path,
    user_path: &str,
) -> std::result::Result<PathBuf, String> {
    let candidate = if std::path::Path::new(user_path).is_absolute() {
        PathBuf::from(user_path)
    } else {
        working_dir.join(user_path)
    };

    let resolved = if candidate.exists() {
        candidate
            .canonicalize()
            .map_err(|e| format!("cannot resolve path '{}': {e}", candidate.display()))?
    } else {
        // File does not exist yet — canonicalize the nearest existing ancestor.
        let mut ancestor = candidate.clone();
        loop {
            if !ancestor.pop() {
                // Reached filesystem root without finding an existing dir.
                return Err(format!("no existing ancestor for '{user_path}'"));
            }
            if ancestor.exists() {
                let canon = ancestor.canonicalize().map_err(|e| {
                    format!("cannot resolve ancestor '{}': {e}", ancestor.display())
                })?;
                // Re-append the remaining components.
                let suffix = candidate
                    .strip_prefix(&ancestor)
                    .map_err(|e| format!("path error: {e}"))?;
                break canon.join(suffix);
            }
        }
    };

    let canon_wd = working_dir
        .canonicalize()
        .map_err(|e| format!("cannot resolve working directory: {e}"))?;

    if !resolved.starts_with(&canon_wd) {
        return Err(format!("path escapes working directory: '{user_path}'"));
    }

    Ok(resolved)
}

impl ToolOutput {
    /// Create a successful (non-error) output.
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            metadata: None,
            files: Vec::new(),
        }
    }

    /// Create an error output.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            metadata: None,
            files: Vec::new(),
        }
    }

    /// Attach a file to be sent to the user via the controller.
    ///
    /// The file is delivered after the text content.  It does not appear
    /// in the LLM's conversation history — it is a side-channel to the
    /// user only.
    pub fn with_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.files.push(path.into());
        self
    }

    /// Attach multiple files to be sent to the user.
    pub fn with_files(mut self, paths: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        self.files.extend(paths.into_iter().map(Into::into));
        self
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_has_no_files() {
        let output = ToolOutput::success("hello");
        assert_eq!(output.content, "hello");
        assert!(!output.is_error);
        assert!(output.files.is_empty());
    }

    #[test]
    fn error_has_no_files() {
        let output = ToolOutput::error("oops");
        assert_eq!(output.content, "oops");
        assert!(output.is_error);
        assert!(output.files.is_empty());
    }

    #[test]
    fn with_file_attaches_single_file() {
        let output = ToolOutput::success("done")
            .with_file("/tmp/report.pdf");
        assert_eq!(output.files.len(), 1);
        assert_eq!(output.files[0], PathBuf::from("/tmp/report.pdf"));
    }

    #[test]
    fn with_file_chains() {
        let output = ToolOutput::success("done")
            .with_file("/tmp/a.txt")
            .with_file("/tmp/b.txt");
        assert_eq!(output.files.len(), 2);
        assert_eq!(output.files[0], PathBuf::from("/tmp/a.txt"));
        assert_eq!(output.files[1], PathBuf::from("/tmp/b.txt"));
    }

    #[test]
    fn with_files_attaches_multiple() {
        let paths = vec!["/tmp/a.txt", "/tmp/b.txt", "/tmp/c.txt"];
        let output = ToolOutput::success("done").with_files(paths);
        assert_eq!(output.files.len(), 3);
        assert_eq!(output.files[2], PathBuf::from("/tmp/c.txt"));
    }

    #[test]
    fn with_file_on_error_output() {
        let output = ToolOutput::error("failed but here's a log")
            .with_file("/tmp/debug.log");
        assert!(output.is_error);
        assert_eq!(output.files.len(), 1);
    }
}
