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
//   mod.rs       — Tool trait, ToolContext, ToolOutput (this file)
//   bash.rs      — Shell execution tool
//   workspace.rs — Unified view/list/search/update for workspace files
//
// How tools fit into the architecture:
//
//   Skill (owns tools)
//     │
//     ├── Arc<dyn Tool>  ─── BashTool
//     ├── Arc<dyn Tool>  ─── WorkspaceTool  (view / list / search / update)
//     └── Arc<dyn Tool>  ─── McpRemoteTool  (MCP servers)
//           │
//           ▼
//   Agent (flat lookup: HashMap<name, Arc<dyn Tool>>)
//     │
//     ▼
//   Sandbox.check(name, input, ctx)  ← gates every call
//     │
//     ▼
//   tool.run(&input, ctx)  → ToolOutput
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
pub mod bulk_edit;
pub mod dependency_scan;
pub mod edit_file;
pub mod export_conversation;
pub mod image_generate;
pub mod kb_search;
pub mod kb_status;
pub mod list_files;
pub mod load_skill;
pub mod memory_search;
pub mod read_file;
pub mod search_files;
pub mod security;
pub mod send_file;
pub mod skill_create;
pub mod view;
pub mod web_fetch;
pub mod web_search;
pub mod workspace;
pub mod write_file;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::error::{DysonError, Result};

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
///     async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
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
    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput>;
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
    pub workspace: Option<crate::workspace::WorkspaceHandle>,

    /// Subagent nesting depth.  0 = top-level agent.
    ///
    /// Used to prevent infinite recursion when subagents spawn subagents.
    /// The subagent tool checks this before spawning a child agent and
    /// returns an error if `depth >= MAX_SUBAGENT_DEPTH`.
    ///
    /// Flows through `ToolContext` (not `AgentSettings`) because depth is
    /// runtime state, not configuration — you don't set it in dyson.json.
    pub depth: u8,

    /// When true, tools skip working-directory path validation.
    ///
    /// Set from `--dangerous-no-sandbox` on the CLI.  Allows tools like
    /// `send_file` to access paths outside the working directory.
    pub dangerous_no_sandbox: bool,

    /// Per-language symbol index cache for `taint_trace`.  Keyed by
    /// `LanguageConfig.display_name`.  Lazily populated on first call;
    /// invalidated via mtime check inside the tool.
    ///
    /// Shared across all tool calls within an agent session to avoid
    /// re-walking the codebase for each trace — a typical security
    /// review issues many taint_trace calls against the same language.
    pub taint_indexes: Arc<RwLock<HashMap<&'static str, Arc<crate::ast::taint::SymbolIndex>>>>,

    /// UI-only activity sink.  When populated (currently only by the
    /// HTTP controller), orchestrator / subagent tools record a
    /// `Running` entry before dispatching the child agent and flip
    /// it to `Ok` / `Err` on return.  Powers the Activity tab's
    /// "Subagents" lane.
    ///
    /// Side-channel: never flows into the LLM conversation.  See
    /// `crates/dyson/src/skill/subagent/mod.rs` on `CaptureOutput`
    /// for the LLM-boundary invariant this respects.
    pub activity: Option<crate::controller::ActivityHandle>,

    /// The id of the tool call currently executing.  Set by the agent's
    /// per-call dispatch (`agent::execution::execute_tool_call`) just
    /// before calling `tool.run`, on a per-call clone of the context so
    /// parallel calls each see their own id.  `None` outside a tool
    /// dispatch (e.g. tools called from tests, dream callbacks).
    ///
    /// Subagent tools propagate this into `ChildSpawn.parent_tool_id`
    /// so the inner agent's `CaptureOutput` can tag every nested SSE
    /// event with its owning subagent box — see
    /// `controller::http::SubagentEventBus`.
    pub tool_use_id: Option<String>,

    /// UI-only side channel for subagents to surface their nested tool
    /// calls live, without those calls flowing into the parent's LLM
    /// conversation.  Only the HTTP controller populates this; other
    /// controllers leave it `None` and subagents run silently as before.
    /// See `controller::http::SubagentEventBus` for the threading
    /// rationale and LLM-boundary invariant.
    pub subagent_events: Option<crate::controller::http::SubagentEventBus>,
}

impl Clone for ToolContext {
    fn clone(&self) -> Self {
        Self {
            working_dir: self.working_dir.clone(),
            env: self.env.clone(),
            cancellation: self.cancellation.clone(),
            workspace: self.workspace.as_ref().map(Arc::clone),
            depth: self.depth,
            dangerous_no_sandbox: self.dangerous_no_sandbox,
            taint_indexes: Arc::clone(&self.taint_indexes),
            activity: self.activity.clone(),
            tool_use_id: self.tool_use_id.clone(),
            subagent_events: self.subagent_events.clone(),
        }
    }
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
            dangerous_no_sandbox: false,
            taint_indexes: Arc::new(RwLock::new(HashMap::new())),
            activity: None,
            tool_use_id: None,
            subagent_events: None,
        })
    }

    /// Create a context rooted at the given directory with no env or workspace.
    ///
    /// Designed for unit tests — avoids repeating the same struct literal
    /// in every tool's test module.
    #[cfg(test)]
    pub fn for_test(dir: &std::path::Path) -> Self {
        Self {
            working_dir: dir.to_path_buf(),
            env: HashMap::new(),
            cancellation: CancellationToken::new(),
            workspace: None,
            depth: 0,
            dangerous_no_sandbox: false,
            taint_indexes: Arc::new(RwLock::new(HashMap::new())),
            activity: None,
            tool_use_id: None,
            subagent_events: None,
        }
    }

    /// Create a test context with a workspace attached.
    ///
    /// Uses `temp_dir()` as working directory. Wraps the workspace in a
    /// `WorkspaceHandle` the way tools expect.
    #[cfg(test)]
    pub fn for_test_with_workspace(ws: impl crate::workspace::Workspace + 'static) -> Self {
        let workspace: Box<dyn crate::workspace::Workspace> = Box::new(ws);
        Self {
            working_dir: std::env::temp_dir(),
            env: HashMap::new(),
            cancellation: CancellationToken::new(),
            workspace: Some(Arc::new(RwLock::new(workspace))),
            depth: 0,
            dangerous_no_sandbox: false,
            taint_indexes: Arc::new(RwLock::new(HashMap::new())),
            activity: None,
            tool_use_id: None,
            subagent_events: None,
        }
    }

    /// Get a reference to the workspace, or return a tool error if not configured.
    pub fn workspace(&self, tool_name: &str) -> Result<&crate::workspace::WorkspaceHandle> {
        self.workspace
            .as_ref()
            .ok_or_else(|| DysonError::tool(tool_name, "no workspace configured"))
    }

    /// Resolve a user-supplied path against this context, honoring
    /// `dangerous_no_sandbox` and expanding `~`.  Returns a ready-to-send
    /// `ToolOutput::error` on failure so callers can `?`-propagate.
    //
    // The `Err` variant here *is* `ToolOutput`, which is intentional: the
    // shape lets every tool's path-validation be a one-liner via `?`.
    // Boxing it would force boilerplate at every callsite, so we accept
    // the larger Err for ergonomic propagation.
    #[allow(clippy::result_large_err)]
    pub fn resolve_path(&self, user_path: &str) -> std::result::Result<PathBuf, ToolOutput> {
        resolve_and_validate_path(&self.working_dir, user_path, self.dangerous_no_sandbox)
            .map_err(ToolOutput::error)
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

    /// Optional typed UI payload — what a controller renders natively.
    ///
    /// Side-channel to the controller (not sent to the LLM).  Tools that
    /// can produce a richer view than plain text (terminal, diff, SBOM,
    /// taint flow, file read) attach a `ToolView` here; the HTTP
    /// controller forwards it over SSE so the right-rail panel renders
    /// the right shape.  Tools that don't bother leave it `None`.
    pub view: Option<view::ToolView>,

    /// Optional structured metadata (not sent to the LLM).
    ///
    /// Used for internal tracking: timing info, exit codes, byte counts,
    /// etc.  Skills can inspect this in their `after_tool()` hook.
    pub metadata: Option<serde_json::Value>,

    /// Files to send to the user via the controller.
    ///
    /// Tools can attach file paths here and the controller's `Output`
    /// implementation will deliver them (e.g., sent as documents,
    /// printed as file paths).  The files are sent
    /// *after* the text content — they are not included in the LLM's
    /// conversation history.
    pub files: Vec<PathBuf>,

    /// Progress checkpoint events produced by this tool call.
    ///
    /// Like `files`, this is a side-channel: checkpoints flow through
    /// the controller's `Output::checkpoint()` hook and do not appear
    /// in the LLM's conversation history.  The default hook drops the
    /// events; controllers that care can override it.
    pub checkpoints: Vec<CheckpointEvent>,

    /// Rendered artefacts produced by this tool call.
    ///
    /// Side-channel to the controller — not sent to the LLM.  Delivered
    /// via `Output::send_artefact`.  Used for outputs that are
    /// "document-shaped" (full-page markdown reports) rather than
    /// chat-shaped (streamed text).  The HTTP controller stores the body
    /// in memory and emits an SSE `artefact` event so the UI can render
    /// it in the Artefacts tab.
    pub artefacts: Vec<crate::message::Artefact>,
}

/// A single progress/checkpoint event emitted by a tool during its run.
///
/// Used only as a side-channel — not serialized into the LLM conversation.
#[derive(Debug, Clone)]
pub struct CheckpointEvent {
    /// Human-readable progress message.
    pub message: String,
    /// Optional fractional progress in the range 0.0..=1.0.
    pub progress: Option<f32>,
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
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
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

/// Resolve a user-supplied path against `working_dir` and, unless
/// `dangerous_no_sandbox` is set, verify it stays inside the workspace.
///
/// Accepts relative or absolute input; `~`/`~/…` always expand to `$HOME`.
/// Existing files are canonicalized (following symlinks); for paths that
/// don't exist yet (e.g. the target of `write_file`), the nearest
/// existing ancestor is canonicalized and the remaining suffix appended.
///
/// When `dangerous_no_sandbox` is true, the escape check is skipped
/// entirely — canonicalizing when possible, returning the lexical path
/// otherwise.
pub fn resolve_and_validate_path(
    working_dir: &std::path::Path,
    user_path: &str,
    dangerous_no_sandbox: bool,
) -> std::result::Result<PathBuf, String> {
    let expanded = crate::util::resolve_tilde(user_path);
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        working_dir.join(expanded)
    };

    if dangerous_no_sandbox {
        return Ok(candidate.canonicalize().unwrap_or(candidate));
    }

    // Try to canonicalize directly — eliminates TOCTOU race between
    // exists() and canonicalize() by going straight to the syscall.
    let resolved = match candidate.canonicalize() {
        Ok(canon) => canon,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // File does not exist yet — canonicalize the nearest existing ancestor.
            let mut ancestor = candidate.clone();
            loop {
                if !ancestor.pop() {
                    // Reached filesystem root without finding an existing dir.
                    return Err(format!("no existing ancestor for '{user_path}'"));
                }
                match ancestor.canonicalize() {
                    Ok(canon) => {
                        // Re-append the remaining components.
                        let suffix = candidate
                            .strip_prefix(&ancestor)
                            .map_err(|e| format!("path error: {e}"))?;
                        break canon.join(suffix);
                    }
                    Err(ref inner) if inner.kind() == std::io::ErrorKind::NotFound => {
                        continue; // ancestor doesn't exist either, keep popping
                    }
                    Err(inner) => {
                        return Err(path_err("resolve ancestor", &ancestor, inner));
                    }
                }
            }
        }
        Err(e) => return Err(path_err("resolve path", &candidate, e)),
    };

    let canon_wd = working_dir
        .canonicalize()
        .map_err(|e| format!("cannot resolve working directory: {e}"))?;

    if !resolved.starts_with(&canon_wd) {
        return Err(format!("path escapes working directory: '{user_path}'"));
    }

    // Defense in depth against TOCTOU in the ancestor-canonicalize branch:
    // between canonicalizing the nearest existing ancestor and this check, an
    // attacker could create a symlink in the suffix that escapes `canon_wd`.
    // If the resolved path now exists, re-canonicalize it and re-verify the
    // bounds so a planted symlink is caught at this layer.
    if resolved.exists() {
        let re_canon = resolved
            .canonicalize()
            .map_err(|e| path_err("re-canonicalize path", &resolved, e))?;
        if !re_canon.starts_with(&canon_wd) {
            return Err(format!("path escapes working directory: '{user_path}'"));
        }
    }

    Ok(resolved)
}

/// Format a path-related error message: `"cannot {verb} '{path}': {err}"`.
pub(crate) fn path_err(verb: &str, path: &std::path::Path, err: impl std::fmt::Display) -> String {
    format!("cannot {verb} '{}': {err}", path.display())
}

impl ToolOutput {
    /// Create a successful (non-error) output.
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            view: None,
            metadata: None,
            files: Vec::new(),
            checkpoints: Vec::new(),
            artefacts: Vec::new(),
        }
    }

    /// Create an error output.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            view: None,
            metadata: None,
            files: Vec::new(),
            checkpoints: Vec::new(),
            artefacts: Vec::new(),
        }
    }

    /// Attach a typed `ToolView` for native UI rendering.
    pub fn with_view(mut self, view: view::ToolView) -> Self {
        self.view = Some(view);
        self
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
    #[cfg(test)]
    pub fn with_files(mut self, paths: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        self.files.extend(paths.into_iter().map(Into::into));
        self
    }

    /// Attach a progress checkpoint event to the output.
    ///
    /// Checkpoints are delivered to the controller's `Output::checkpoint`
    /// hook as a side-channel — they do not appear in the LLM
    /// conversation history.  The default hook drops events; controllers
    /// that need progress reporting can override it.
    pub fn with_checkpoint(mut self, event: CheckpointEvent) -> Self {
        self.checkpoints.push(event);
        self
    }

    /// Attach a rendered artefact to the output.
    ///
    /// Artefacts are delivered to the controller's `Output::send_artefact`
    /// hook as a side-channel — the LLM never sees them.  The HTTP
    /// controller stores the body in an in-memory FIFO store and emits
    /// an SSE `artefact` event so the UI can render it in the Artefacts
    /// tab.  Other controllers' default impl is a no-op.
    pub fn with_artefact(mut self, artefact: crate::message::Artefact) -> Self {
        self.artefacts.push(artefact);
        self
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Truncate a string to `max_chars`, appending "..." if truncated.
pub(crate) fn truncate(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        let mut end = max_chars;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
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
        let output = ToolOutput::success("done").with_file("/tmp/report.pdf");
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
        let output = ToolOutput::error("failed but here's a log").with_file("/tmp/debug.log");
        assert!(output.is_error);
        assert_eq!(output.files.len(), 1);
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let long = "a".repeat(300);
        let result = truncate(&long, 200);
        assert_eq!(result.len(), 203); // 200 + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn dangerous_no_sandbox_accepts_outside_path() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = std::env::temp_dir();
        let outside_str = outside.to_str().unwrap();
        // Without the flag: escape rejected (temp_dir is not inside `tmp`).
        let err = resolve_and_validate_path(tmp.path(), outside_str, false);
        assert!(err.is_err(), "escape check should fire when sandboxed");

        // With the flag: returned unchanged (canonicalized if it exists).
        let ok = resolve_and_validate_path(tmp.path(), outside_str, true);
        assert!(ok.is_ok(), "no-sandbox should skip escape check");
    }

    #[test]
    fn dangerous_no_sandbox_expands_tilde() {
        let tmp = tempfile::tempdir().unwrap();
        // ~ is convenience, not sandboxing — expansion happens regardless.
        let ok = resolve_and_validate_path(tmp.path(), "~", true).unwrap();
        assert!(ok.is_absolute(), "got {ok:?}");
        assert!(!ok.to_string_lossy().contains('~'), "got {ok:?}");
    }
}
