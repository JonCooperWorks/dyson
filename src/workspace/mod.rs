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
//   mod.rs      — Workspace trait + factory (this file)
//   openclaw.rs — OpenClawWorkspace (filesystem, OpenClaw format)
//   in_memory.rs — InMemoryWorkspace (for testing)
//
// Backends:
//   The Workspace trait abstracts storage.  Implementations decide where
//   and how files are stored:
//
//   - OpenClawWorkspace: reads/writes markdown files on the local filesystem
//     in the OpenClaw format.  Compatible with OpenClaw/TARS workspaces.
//   - InMemoryWorkspace: in-memory HashMap, no persistence.  For tests.
//   - (future) Cloud, database, or API-backed workspaces.
//
// Configuration:
//   In dyson.json:
//   ```json
//   {
//     "workspace": {
//       "backend": "openclaw",
//       "connection_string": "~/.dyson"
//     }
//   }
//   ```
//   The connection_string supports the secret resolver scheme:
//   ```json
//   { "connection_string": { "resolver": "insecure_env", "name": "WORKSPACE_DIR" } }
//   ```
// ===========================================================================

pub mod in_memory;
pub mod openclaw;

pub use in_memory::InMemoryWorkspace;
pub use openclaw::OpenClawWorkspace;

use crate::config::WorkspaceConfig;
use crate::error::{DysonError, Result};

// ---------------------------------------------------------------------------
// Workspace trait
// ---------------------------------------------------------------------------

/// Trait abstracting workspace storage backends.
///
/// A workspace holds the agent's persistent identity and memory:
/// SOUL.md, IDENTITY.md, MEMORY.md, journals, etc.  Implementations
/// decide where and how files are stored.
///
/// The agent interacts with the workspace through tools (workspace_view,
/// workspace_search, workspace_update) which call these methods.
pub trait Workspace: Send + Sync {
    /// Get a file's content by name (e.g., "SOUL.md", "memory/2026-03-19.md").
    ///
    /// Returns an owned String to avoid lifetime issues across async boundaries.
    fn get(&self, name: &str) -> Option<String>;

    /// Set a file's content (in memory — call save() to persist).
    fn set(&mut self, name: &str, content: &str);

    /// Append to a file (creates it if it doesn't exist).
    fn append(&mut self, name: &str, content: &str);

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
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Create a workspace from configuration.
///
/// Dispatches on `config.backend` to construct the appropriate implementation.
pub fn create_workspace(config: &WorkspaceConfig) -> Result<Box<dyn Workspace>> {
    match config.backend.as_str() {
        "openclaw" => {
            let ws = OpenClawWorkspace::load_from_connection_string(&config.connection_string)?;
            Ok(Box::new(ws))
        }
        other => Err(DysonError::Config(format!(
            "unknown workspace backend: '{other}'.  Supported: 'openclaw'."
        ))),
    }
}
