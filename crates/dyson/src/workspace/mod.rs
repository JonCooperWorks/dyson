// ===========================================================================
// Workspace — trait for agent identity, memory, and journals.
//
// LEARNING OVERVIEW
//
// What this module does:
//   Defines the Workspace trait and its implementations.  A workspace holds
//   the agent's persistent state: identity (SOUL.md, IDENTITY.md), memory
//   (MEMORY.md), and daily journals (memory/YYYY-MM-DD.md).
//
// Module layout:
//   mod.rs        — Workspace trait + factory (this file)
//   filesystem.rs — FilesystemWorkspace (filesystem, filesystem format)
//   in_memory.rs  — InMemoryWorkspace (for testing)
//
// Backends:
//   The Workspace trait abstracts storage.  Implementations decide where
//   and how files are stored:
//
//   - FilesystemWorkspace: reads/writes markdown files on the local filesystem
//     in the filesystem format.  Compatible with filesystem/TARS workspaces.
//   - InMemoryWorkspace: in-memory HashMap, no persistence.  For tests.
//   - (future) Cloud, database, or API-backed workspaces.
//
// Configuration:
//   In dyson.json:
//   ```json
//   {
//     "workspace": {
//       "backend": "filesystem",
//       "connection_string": "~/.dyson"
//     }
//   }
//   ```
//   The connection_string supports the secret resolver scheme:
//   ```json
//   { "connection_string": { "resolver": "insecure_env", "name": "WORKSPACE_DIR" } }
//   ```
// ===========================================================================

pub mod channel;
pub mod filesystem;
pub mod in_memory;
pub mod memory_store;
pub mod migrate;

pub use filesystem::FilesystemWorkspace;
pub use in_memory::InMemoryWorkspace;

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::config::WorkspaceConfig;
use crate::error::{DysonError, Result};

/// Shared, mutable handle to a `Workspace`.
///
/// Every layer of Dyson that touches the workspace — tools, LLM clients, MCP
/// servers, subagents — holds this same Arc so memory reads and writes stay
/// consistent across concurrent tool calls.  The RwLock is fine-grained:
/// reads (view/list/search) proceed in parallel, updates take exclusive
/// access.
pub type WorkspaceHandle = Arc<RwLock<Box<dyn Workspace>>>;

// ---------------------------------------------------------------------------
// Workspace trait
// ---------------------------------------------------------------------------

/// Trait abstracting workspace storage backends.
///
/// A workspace holds the agent's persistent identity and memory:
/// SOUL.md, IDENTITY.md, MEMORY.md, journals, etc.  Implementations
/// decide where and how files are stored.
///
/// The agent interacts with the workspace through the unified `workspace`
/// tool (ops: view/list/search/update) which calls these methods.
pub trait Workspace: Send + Sync {
    /// Get a file's content by name (e.g., "SOUL.md", "memory/2026-03-19.md").
    ///
    /// Returns an owned String to avoid lifetime issues across async boundaries.
    fn get(&self, name: &str) -> Option<String>;

    /// Set a file's content (in memory — call save() to persist).
    fn set(&mut self, name: &str, content: &str);

    /// Append to a file (creates it if it doesn't exist).
    fn append(&mut self, name: &str, content: &str);

    /// Remove a file from the workspace.
    ///
    /// Returns true when the file existed in memory or on disk. Backends that
    /// cannot delete files can keep the default no-op behavior.
    fn remove(&mut self, _name: &str) -> Result<bool> {
        Ok(false)
    }

    /// Persist all pending changes to the backing store.
    fn save(&self) -> Result<()>;

    /// List all file names in the workspace.
    fn list_files(&self) -> Vec<String>;

    /// Search files for a pattern, returning (filename, matching_lines) pairs.
    ///
    /// Supports regex patterns (case-insensitive).  If the pattern is not
    /// valid regex, falls back to literal substring match.
    fn search(&self, pattern: &str) -> Vec<(String, Vec<String>)>;

    /// Build the system prompt fragment from workspace files.
    ///
    /// Composes the agent's context from identity files (SOUL.md, IDENTITY.md,
    /// MEMORY.md) and recent journals.
    fn system_prompt(&self) -> String;

    /// Append to today's journal.
    fn journal(&mut self, entry: &str);

    /// Soft character target for a given file, or None if unlimited.
    ///
    /// This is a **soft** target — curation aims here but is allowed to
    /// overflow up to `char_ceiling()` when the extra chars carry signal.
    fn char_limit(&self, _file: &str) -> Option<usize> {
        None
    }

    /// Hard character ceiling for a given file, or None if unlimited.
    ///
    /// Writes up to this number of characters are accepted (with a warning
    /// in the overflow band between `char_limit` and `char_ceiling`).
    /// Writes above the ceiling are rejected outright.
    ///
    /// Default implementation returns `char_limit()` — backends that
    /// support fuzzy limits override this to return the ceiling.
    fn char_ceiling(&self, file: &str) -> Option<usize> {
        self.char_limit(file)
    }

    /// How often (in turns) to inject a memory maintenance nudge.  0 = disabled.
    fn nudge_interval(&self) -> usize {
        5
    }

    /// Full-text search over memory files (Tier 2).
    ///
    /// Returns `(file_key, snippet)` pairs.  Default implementation
    /// returns empty — backends with FTS5 override this.
    fn memory_search(&self, _query: &str) -> Vec<(String, String)> {
        vec![]
    }

    /// Discover skill directories in the workspace's `skills/` directory.
    ///
    /// Returns absolute paths to skill directories (e.g., `skills/code-review/`)
    /// that contain a `SKILL.md` file.  Each skill lives in its own directory
    /// to support references, scripts, and examples alongside the skill file.
    ///
    /// Default implementation returns empty (InMemoryWorkspace, etc.).
    fn skill_dirs(&self) -> Vec<std::path::PathBuf> {
        vec![]
    }

    /// Directory where coding projects are stored.
    ///
    /// Coding tools (read_file, write_file, edit_file, etc.) use this as
    /// their working directory.  The agent can create and manage projects
    /// within this directory.
    ///
    /// Default implementation returns `None` (coding tools fall back to
    /// the process's current working directory).
    fn programs_dir(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// Set the current user attribution for write auditing.
    ///
    /// When set, implementations may record who triggered each write
    /// operation.  Pass `None` to clear (e.g., for dream/system writes).
    ///
    /// Default implementation is a no-op — only `ChannelWorkspace`
    /// (public agents) tracks attribution.
    fn set_attribution(&mut self, _user: Option<&str>) {}
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Create a workspace from configuration.
///
/// Dispatches on `config.backend` to construct the appropriate implementation.
pub fn create_workspace(config: &WorkspaceConfig) -> Result<Box<dyn Workspace>> {
    match config.backend.as_str() {
        "filesystem" => {
            let ws = FilesystemWorkspace::load_from_connection_string(
                config.connection_string.expose(),
                config.memory.clone(),
            )?;
            Ok(Box::new(ws))
        }
        other => Err(DysonError::Config(format!(
            "unknown workspace backend: '{other}'.  Supported: 'filesystem'."
        ))),
    }
}

/// Create a per-channel workspace under `{main_workspace}/channels/{channel_id}/`.
///
/// Channel workspaces give public agents persistent memory scoped to a single
/// channel (e.g. a Telegram group chat).  SOUL.md and IDENTITY.md are symlinked
/// to the main workspace so identity changes propagate automatically.  The
/// The returned workspace is wrapped in a [`ChannelWorkspace`] that silently
/// drops writes to the protected identity keys.
///
/// On first creation:
/// 1. The `channels/{channel_id}/` directory is created.
/// 2. SOUL.md and IDENTITY.md are symlinked to the main workspace's copies.
/// 3. `FilesystemWorkspace::load()` creates default MEMORY.md, etc.
///
/// On subsequent loads, existing symlinks are left in place.
pub fn create_channel_workspace(
    config: &WorkspaceConfig,
    channel_id: &str,
) -> Result<Box<dyn Workspace>> {
    let main_path = crate::util::resolve_tilde(config.connection_string.expose());
    let channel_path = main_path.join("channels").join(channel_id);

    // Create the channel directory if it doesn't exist.
    std::fs::create_dir_all(&channel_path).map_err(|e| {
        DysonError::Config(format!(
            "cannot create channel workspace at {}: {e}",
            channel_path.display()
        ))
    })?;

    // Symlink identity files from the main workspace.  Skip if the symlink
    // already exists or the source file is missing.
    for file in ["SOUL.md", "IDENTITY.md"] {
        let target = channel_path.join(file);
        let source = main_path.join(file);
        if !target.exists()
            && source.exists()
            && let Err(e) = std::os::unix::fs::symlink(&source, &target)
        {
            tracing::warn!(
                file,
                source = %source.display(),
                target = %target.display(),
                error = %e,
                "failed to symlink identity file into channel workspace"
            );
        }
    }

    let ws = FilesystemWorkspace::load(&channel_path, config.memory.clone())?;

    // Wrap in ChannelWorkspace — only explicitly allowed keys are writable.
    // Everything else (SOUL.md, IDENTITY.md, AGENTS.md, etc.) is protected
    // by default.  This prevents prompt injection from modifying identity
    // and prevents writes from flowing through symlinks to the main workspace.
    let mut ws = channel::ChannelWorkspace::new(Box::new(ws))
        .allow("MEMORY.md")
        .allow("USER.md")
        .allow_prefix("memory/");

    // Prune old journal files to bound storage growth.
    ws.expire_journals();

    Ok(Box::new(ws))
}
