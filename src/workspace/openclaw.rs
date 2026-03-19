// ===========================================================================
// OpenClawWorkspace — OpenClaw-compatible agent memory and identity.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Manages the agent's persistent state: identity, personality, memory,
//   and daily journals.  Uses the OpenClaw file format so Dyson can be a
//   drop-in replacement for OpenClaw-powered agents like TARS.
//
// File layout (~/.dyson/ by default):
//
//   ~/.dyson/
//     SOUL.md          — personality, vibe, behavioral guidelines
//     IDENTITY.md      — who the agent is, what it runs, capabilities
//     MEMORY.md        — curated long-term memory (updated by the agent)
//     AGENTS.md        — operating procedures (session startup, safety rules)
//     HEARTBEAT.md     — periodic task checklist (for future heartbeat support)
//     memory/
//       2026-03-19.md  — daily journal (one per day, created automatically)
//       2026-03-18.md
//       ...
//
// OpenClaw compatibility:
//   These files are the same format as OpenClaw/TARS.  If you have an
//   existing OpenClaw workspace, point Dyson at it and it reads the same
//   files.  If you don't, Dyson creates sensible defaults.
// ===========================================================================

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{DysonError, Result};
use crate::workspace::Workspace;

// ---------------------------------------------------------------------------
// OpenClawWorkspace — the persistent state directory.
// ---------------------------------------------------------------------------

/// The agent's persistent workspace — identity, memory, and journals.
///
/// Reads/writes markdown files in the OpenClaw format.  The workspace
/// directory defaults to `~/.dyson/` but can be configured.
pub struct OpenClawWorkspace {
    /// Root directory of the workspace.
    path: PathBuf,

    /// Loaded file contents, keyed by filename (e.g., "SOUL.md").
    files: HashMap<String, String>,
}

impl OpenClawWorkspace {
    /// Load a workspace from a directory.
    ///
    /// Creates the directory and default files if they don't exist.
    /// Reads all .md files in the root and the memory/ subdirectory.
    pub fn load(path: &Path) -> Result<Self> {
        // Create the directory structure if it doesn't exist.
        std::fs::create_dir_all(path).map_err(|e| {
            DysonError::Config(format!(
                "cannot create workspace dir {}: {e}",
                path.display()
            ))
        })?;
        std::fs::create_dir_all(path.join("memory")).map_err(|e| {
            DysonError::Config(format!("cannot create memory dir: {e}"))
        })?;

        let mut files = HashMap::new();

        // Read top-level .md files.
        for entry in std::fs::read_dir(path).map_err(|e| {
            DysonError::Config(format!("cannot read workspace dir: {e}"))
        })? {
            let entry = entry.map_err(|e| DysonError::Io(e))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".md") && entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                let content = std::fs::read_to_string(entry.path())?;
                files.insert(name, content);
            }
        }

        // Read memory/ journal files.
        let memory_dir = path.join("memory");
        if memory_dir.exists() {
            for entry in std::fs::read_dir(&memory_dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") {
                    let content = std::fs::read_to_string(entry.path())?;
                    files.insert(format!("memory/{name}"), content);
                }
            }
        }

        // Create default files if they don't exist.
        let mut workspace = Self {
            path: path.to_path_buf(),
            files,
        };
        workspace.ensure_defaults()?;

        tracing::info!(
            path = %path.display(),
            files = workspace.files.len(),
            "workspace loaded"
        );

        Ok(workspace)
    }

    /// Resolve the workspace path without loading it.
    ///
    /// Used by the hot reloader to know which directory to watch.
    pub fn resolve_path(config_path: Option<&str>) -> Option<PathBuf> {
        let home = std::env::var("HOME").unwrap_or_default();
        let path = match config_path {
            Some(p) if p.starts_with("~/") => PathBuf::from(&home).join(&p[2..]),
            Some(p) if p == "~" => PathBuf::from(&home),
            Some(p) => PathBuf::from(p),
            None => PathBuf::from(&home).join(".dyson"),
        };
        if path.exists() { Some(path) } else { None }
    }

    /// Load from the default path (~/.dyson/) or a configured path.
    pub fn load_default(config_path: Option<&str>) -> Result<Self> {
        let path = resolve_tilde(config_path.unwrap_or("~/.dyson"));
        Self::load(&path)
    }

    /// Load from a connection string (path with ~ expansion).
    pub fn load_from_connection_string(connection_string: &str) -> Result<Self> {
        let path = resolve_tilde(connection_string);
        Self::load(&path)
    }

    /// The workspace directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get today's date as YYYY-MM-DD.
    pub fn today_date() -> String {
        chrono_today()
    }

    /// Get today's journal file name.
    pub fn today_journal() -> String {
        let now = chrono_today();
        format!("memory/{now}.md")
    }

    /// Get yesterday's journal file name.
    pub fn yesterday_journal() -> String {
        let yesterday = chrono_yesterday();
        format!("memory/{yesterday}.md")
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    /// Create default files if they don't exist.
    fn ensure_defaults(&mut self) -> Result<()> {
        if !self.files.contains_key("SOUL.md") {
            let default = "\
# SOUL.md — Who You Are

Be genuinely helpful, not performatively helpful. Skip the filler words — just help.

Have opinions. Be resourceful before asking. Earn trust through competence.

Update this file as you learn who you are.
";
            self.files.insert("SOUL.md".into(), default.into());
            std::fs::write(self.path.join("SOUL.md"), default)?;
        }

        if !self.files.contains_key("IDENTITY.md") {
            let default = "\
# IDENTITY.md — Who Am I?

- **Name:** Dyson
- **Mode:** AI assistant
- **Powered by:** Dyson agent framework

Update this file with your specific identity, capabilities, and context.
";
            self.files.insert("IDENTITY.md".into(), default.into());
            std::fs::write(self.path.join("IDENTITY.md"), default)?;
        }

        if !self.files.contains_key("AGENTS.md") {
            let default = "\
# AGENTS.md — Operating Procedures

## Every Session

Before doing anything else:
1. Read SOUL.md — this is who you are
2. Read IDENTITY.md — this is your context
3. Read today's journal (memory/YYYY-MM-DD.md) for recent context
4. Read MEMORY.md for long-term context

## Memory

You wake up fresh each session. These files are your continuity:
- **Daily notes:** memory/YYYY-MM-DD.md — raw logs of what happened
- **Long-term:** MEMORY.md — curated memories

Capture what matters. Decisions, context, things to remember.
";
            self.files.insert("AGENTS.md".into(), default.into());
            std::fs::write(self.path.join("AGENTS.md"), default)?;
        }

        if !self.files.contains_key("MEMORY.md") {
            let default = "\
# MEMORY.md — Long-Term Memory

*Nothing here yet. Update this file as you learn things worth remembering.*
";
            self.files.insert("MEMORY.md".into(), default.into());
            std::fs::write(self.path.join("MEMORY.md"), default)?;
        }

        if !self.files.contains_key("HEARTBEAT.md") {
            let default = "\
# HEARTBEAT.md

# Keep this file empty to skip heartbeat tasks.
# Add tasks below when you want the agent to check something periodically.
";
            self.files.insert("HEARTBEAT.md".into(), default.into());
            std::fs::write(self.path.join("HEARTBEAT.md"), default)?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Workspace trait implementation
// ---------------------------------------------------------------------------

impl Workspace for OpenClawWorkspace {
    fn get(&self, name: &str) -> Option<String> {
        self.files.get(name).cloned()
    }

    fn set(&mut self, name: &str, content: &str) {
        self.files.insert(name.to_string(), content.to_string());
    }

    fn append(&mut self, name: &str, content: &str) {
        let entry = self.files.entry(name.to_string()).or_default();
        if !entry.is_empty() && !entry.ends_with('\n') {
            entry.push('\n');
        }
        entry.push_str(content);
    }

    fn save(&self) -> Result<()> {
        for (name, content) in &self.files {
            let file_path = self.path.join(name);

            // Ensure parent directory exists (for memory/ files).
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            std::fs::write(&file_path, content)?;
        }

        tracing::debug!(
            files = self.files.len(),
            "workspace saved"
        );

        Ok(())
    }

    fn list_files(&self) -> Vec<String> {
        let mut names: Vec<String> = self.files.keys().cloned().collect();
        names.sort();
        names
    }

    fn search(&self, pattern: &str) -> Vec<(String, Vec<String>)> {
        let pattern_lower = pattern.to_lowercase();
        let mut results = Vec::new();

        for (name, content) in &self.files {
            let matching_lines: Vec<String> = content
                .lines()
                .filter(|line| line.to_lowercase().contains(&pattern_lower))
                .map(|line| line.to_string())
                .collect();

            if !matching_lines.is_empty() {
                results.push((name.clone(), matching_lines));
            }
        }

        results.sort_by(|a, b| a.0.cmp(&b.0));
        results
    }

    fn system_prompt(&self) -> String {
        let mut parts: Vec<String> = Vec::new();

        // Core identity files — always loaded.
        for (label, file) in [
            ("PERSONALITY", "SOUL.md"),
            ("IDENTITY", "IDENTITY.md"),
            ("LONG-TERM MEMORY", "MEMORY.md"),
        ] {
            if let Some(content) = self.files.get(file) {
                if !content.trim().is_empty() {
                    parts.push(format!("## {label}\n\n{content}"));
                }
            }
        }

        // Yesterday's journal (for continuity across sessions).
        let yesterday = Self::yesterday_journal();
        if let Some(content) = self.files.get(&yesterday) {
            if !content.trim().is_empty() {
                parts.push(format!("## YESTERDAY'S JOURNAL\n\n{content}"));
            }
        }

        // Today's journal.
        let today = Self::today_journal();
        if let Some(content) = self.files.get(&today) {
            if !content.trim().is_empty() {
                parts.push(format!("## TODAY'S JOURNAL\n\n{content}"));
            }
        }

        parts.join("\n\n---\n\n")
    }

    fn journal(&mut self, entry: &str) {
        let name = Self::today_journal();
        self.append(&name, entry);
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Resolve ~ to $HOME in a path string.
pub(crate) fn resolve_tilde(path: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    if path.starts_with("~/") {
        PathBuf::from(&home).join(&path[2..])
    } else if path == "~" {
        PathBuf::from(&home)
    } else {
        PathBuf::from(path)
    }
}

// ---------------------------------------------------------------------------
// Date helpers
// ---------------------------------------------------------------------------

/// Get today's date as YYYY-MM-DD string.
pub(crate) fn chrono_today() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    unix_to_date(now)
}

/// Get yesterday's date as YYYY-MM-DD string.
fn chrono_yesterday() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    unix_to_date(now.saturating_sub(86400))
}

/// Convert Unix timestamp to YYYY-MM-DD string.
fn unix_to_date(secs: u64) -> String {
    let z = (secs / 86400) as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}")
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_workspace() -> (PathBuf, OpenClawWorkspace) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "dyson-test-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = OpenClawWorkspace::load(&dir).unwrap();
        (dir, ws)
    }

    #[test]
    fn creates_default_files() {
        let (dir, ws) = temp_workspace();
        assert!(ws.get("SOUL.md").is_some());
        assert!(ws.get("IDENTITY.md").is_some());
        assert!(ws.get("AGENTS.md").is_some());
        assert!(ws.get("MEMORY.md").is_some());
        assert!(ws.get("HEARTBEAT.md").is_some());
        assert!(dir.join("SOUL.md").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preserves_existing_files() {
        let (dir, _) = temp_workspace();

        // Write a custom SOUL.md.
        std::fs::write(dir.join("SOUL.md"), "I am custom").unwrap();

        // Reload — should keep the custom content.
        let ws = OpenClawWorkspace::load(&dir).unwrap();
        assert_eq!(ws.get("SOUL.md").unwrap(), "I am custom");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_appends() {
        let (dir, mut ws) = temp_workspace();
        ws.journal("## Session started");
        ws.journal("Did some work");

        let today = OpenClawWorkspace::today_journal();
        let content = ws.get(&today).unwrap();
        assert!(content.contains("Session started"));
        assert!(content.contains("Did some work"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_persists_to_disk() {
        let (dir, mut ws) = temp_workspace();
        ws.set("MEMORY.md", "# Updated memory\n\nI learned something.");
        ws.save().unwrap();

        let on_disk = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        assert!(on_disk.contains("I learned something"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_prompt_composes_files() {
        let (dir, ws) = temp_workspace();
        let prompt = ws.system_prompt();
        assert!(prompt.contains("PERSONALITY"));
        assert!(prompt.contains("IDENTITY"));
        assert!(prompt.contains("LONG-TERM MEMORY"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn today_date_format() {
        let today = chrono_today();
        assert_eq!(today.len(), 10);
        assert_eq!(&today[4..5], "-");
        assert_eq!(&today[7..8], "-");
    }

    #[test]
    fn list_files_returns_sorted() {
        let (dir, ws) = temp_workspace();
        let files = ws.list_files();
        assert!(files.contains(&"SOUL.md".to_string()));
        assert!(files.contains(&"MEMORY.md".to_string()));
        // Verify sorted.
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_finds_matches() {
        let (dir, mut ws) = temp_workspace();
        ws.set("MEMORY.md", "# Memory\n\nI learned about Rust.\nRust is great.");
        let results = ws.search("rust");
        assert!(!results.is_empty());
        let (name, lines) = &results.iter().find(|(n, _)| n == "MEMORY.md").unwrap();
        assert_eq!(name, "MEMORY.md");
        assert_eq!(lines.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
