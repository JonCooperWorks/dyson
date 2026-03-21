// ===========================================================================
// Hot reload — re-read config and workspace when files change on disk.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Watches files by mtime (last modification time).  Before each agent
//   turn, the controller calls `check()` — if any watched file changed,
//   it signals a reload.
//
// Watches two things:
//   1. dyson.json — agent settings, sandbox config, etc.
//   2. Workspace files — SOUL.md, MEMORY.md, IDENTITY.md, etc.
//
// Why mtime instead of a file watcher (inotify/FSEvents)?
//   - Zero dependencies
//   - Works on all platforms
//   - Polling once per agent turn is cheap (a few stat() syscalls)
// ===========================================================================

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::Settings;
use crate::error::Result;

// ---------------------------------------------------------------------------
// HotReloader
// ---------------------------------------------------------------------------

/// Watches config and workspace files for changes.
///
/// Call `check()` before each agent turn.  Returns `true` if anything
/// changed (config or workspace files), signaling the controller to
/// rebuild the agent.
pub struct HotReloader {
    /// Watched files and their last known mtimes.
    watched: HashMap<PathBuf, Option<SystemTime>>,

    /// Path to the config file (for reloading settings).
    config_path: Option<PathBuf>,
}

impl HotReloader {
    /// Create a reloader that watches the config file and workspace directory.
    pub fn new(config_path: Option<&Path>, workspace_path: Option<&Path>) -> Self {
        let mut watched = HashMap::new();

        // Watch the config file.
        if let Some(p) = config_path {
            let mtime = Self::get_mtime(p);
            watched.insert(p.to_path_buf(), mtime);
        }

        // Watch workspace .md files.
        if let Some(ws) = workspace_path {
            Self::scan_workspace(ws, &mut watched);
        }

        Self {
            watched,
            config_path: config_path.map(|p| p.to_path_buf()),
        }
    }

    /// Check if any watched file changed.
    ///
    /// Returns `Ok(Some(settings))` if the config file changed (new settings).
    /// Returns `Ok(None)` with `changed == true` if only workspace files changed.
    /// The bool in the tuple indicates whether a rebuild is needed.
    pub fn check(&mut self) -> Result<(bool, Option<Settings>)> {
        let mut changed = false;
        let mut new_settings = None;

        for (path, last_mtime) in self.watched.iter_mut() {
            let current_mtime = Self::get_mtime(path);
            if current_mtime != *last_mtime {
                tracing::info!(path = %path.display(), "file changed");
                *last_mtime = current_mtime;
                changed = true;
            }
        }

        // If the config file specifically changed, reload settings.
        if changed && let Some(ref config_path) = self.config_path && config_path.exists() {
            match crate::config::loader::load_settings(Some(config_path)) {
                Ok(s) => new_settings = Some(s),
                Err(e) => {
                    tracing::warn!(error = %e, "config reload failed — keeping old");
                }
            }
        }

        Ok((changed, new_settings))
    }

    fn get_mtime(path: &Path) -> Option<SystemTime> {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
    }

    fn scan_workspace(ws_path: &Path, watched: &mut HashMap<PathBuf, Option<SystemTime>>) {
        // Watch top-level .md files.
        if let Ok(entries) = std::fs::read_dir(ws_path) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") {
                    let p = entry.path();
                    let mtime = Self::get_mtime(&p);
                    watched.insert(p, mtime);
                }
            }
        }

        // Watch memory/ journal files.
        let memory_dir = ws_path.join("memory");
        if let Ok(entries) = std::fs::read_dir(&memory_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") {
                    let p = entry.path();
                    let mtime = Self::get_mtime(&p);
                    watched.insert(p, mtime);
                }
            }
        }
    }
}
