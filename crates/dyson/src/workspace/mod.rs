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
pub mod in_memory;
pub mod memory_store;
pub mod migrate;
pub mod filesystem;

pub use in_memory::InMemoryWorkspace;
pub use filesystem::FilesystemWorkspace;

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
// WorkspaceMigrate — trait for migrating between workspace backends.
// Only used in tests for now; will be promoted when backend migration ships.
// ---------------------------------------------------------------------------

#[cfg(test)]
pub struct MigrationResult {
    pub files_migrated: usize,
    pub file_names: Vec<String>,
}

#[cfg(test)]
pub trait WorkspaceMigrate: Workspace {
    fn export_files(&self) -> Vec<(String, String)> {
        self.list_files()
            .into_iter()
            .filter_map(|name| {
                let content = self.get(&name)?;
                Some((name, content))
            })
            .collect()
    }

    fn import_files(&mut self, files: &[(String, String)]) {
        for (name, content) in files {
            self.set(name, content);
        }
    }
}

#[cfg(test)]
impl<T: Workspace + ?Sized> WorkspaceMigrate for T {}

#[cfg(test)]
pub fn migrate_workspace(
    source: &dyn Workspace,
    target: &mut dyn Workspace,
) -> Result<MigrationResult> {
    let files = source.export_files();
    let file_names: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
    let count = files.len();

    target.import_files(&files);
    target.save()?;

    Ok(MigrationResult {
        files_migrated: count,
        file_names,
    })
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
        if !target.exists() && source.exists()
            && let Err(e) = std::os::unix::fs::symlink(&source, &target) {
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

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_between_in_memory_workspaces() {
        let source = InMemoryWorkspace::new()
            .with_file("SOUL.md", "Be bold.")
            .with_file("MEMORY.md", "Learned Rust.")
            .with_file("memory/2026-03-19.md", "Built a feature.");

        let mut target = InMemoryWorkspace::new();

        let result = migrate_workspace(&source, &mut target).unwrap();

        assert_eq!(result.files_migrated, 3);
        assert_eq!(target.get("SOUL.md").unwrap(), "Be bold.");
        assert_eq!(target.get("MEMORY.md").unwrap(), "Learned Rust.");
        assert_eq!(
            target.get("memory/2026-03-19.md").unwrap(),
            "Built a feature."
        );
    }

    #[test]
    fn migrate_filesystem_to_in_memory() {
        let dir = std::env::temp_dir().join(format!("dyson-migrate-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut source =
            FilesystemWorkspace::load(&dir, crate::config::MemoryConfig::default()).unwrap();
        source.set("SOUL.md", "Custom soul.");
        source.set("MEMORY.md", "Important memory.");
        source.save().unwrap();

        let mut target = InMemoryWorkspace::new();
        let result = migrate_workspace(&source, &mut target).unwrap();

        assert!(result.files_migrated >= 2);
        assert_eq!(target.get("SOUL.md").unwrap(), "Custom soul.");
        assert_eq!(target.get("MEMORY.md").unwrap(), "Important memory.");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_in_memory_to_filesystem() {
        let source = InMemoryWorkspace::new()
            .with_file("SOUL.md", "Migrated soul.")
            .with_file("memory/2026-03-20.md", "Journal entry.");

        let dir = std::env::temp_dir().join(format!("dyson-migrate-target-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut target =
            FilesystemWorkspace::load(&dir, crate::config::MemoryConfig::default()).unwrap();
        let result = migrate_workspace(&source, &mut target).unwrap();

        assert_eq!(result.files_migrated, 2);

        // Verify persisted to disk.
        let on_disk = std::fs::read_to_string(dir.join("SOUL.md")).unwrap();
        assert_eq!(on_disk, "Migrated soul.");
        let journal = std::fs::read_to_string(dir.join("memory/2026-03-20.md")).unwrap();
        assert_eq!(journal, "Journal entry.");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_empty_workspace() {
        let source = InMemoryWorkspace::new();
        let mut target = InMemoryWorkspace::new();

        let result = migrate_workspace(&source, &mut target).unwrap();

        assert_eq!(result.files_migrated, 0);
        assert!(result.file_names.is_empty());
        assert!(target.list_files().is_empty());
    }

    #[test]
    fn migrate_overwrites_existing_target_files() {
        let source = InMemoryWorkspace::new().with_file("SOUL.md", "New soul.");

        let mut target = InMemoryWorkspace::new().with_file("SOUL.md", "Old soul.");

        migrate_workspace(&source, &mut target).unwrap();

        assert_eq!(target.get("SOUL.md").unwrap(), "New soul.");
    }

    #[test]
    fn export_files_returns_all_files() {
        let ws = InMemoryWorkspace::new()
            .with_file("A.md", "aaa")
            .with_file("B.md", "bbb");

        let files = ws.export_files();
        assert_eq!(files.len(), 2);

        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"A.md"));
        assert!(names.contains(&"B.md"));
    }

    #[test]
    fn import_files_sets_content() {
        let mut ws = InMemoryWorkspace::new();
        let files = vec![
            ("X.md".to_string(), "xxx".to_string()),
            ("Y.md".to_string(), "yyy".to_string()),
        ];

        ws.import_files(&files);

        assert_eq!(ws.get("X.md").unwrap(), "xxx");
        assert_eq!(ws.get("Y.md").unwrap(), "yyy");
    }
}
