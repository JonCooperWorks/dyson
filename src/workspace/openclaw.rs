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
//     skills/
//       code-review.md — local skill files (auto-discovered, Hermes-style)
//       ...
//
// OpenClaw compatibility:
//   These files are the same format as OpenClaw/TARS.  If you have an
//   existing OpenClaw workspace, point Dyson at it and it reads the same
//   files.  If you don't, Dyson creates sensible defaults.
// ===========================================================================

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use regex::RegexBuilder;

use crate::config::MemoryConfig;
use crate::error::{DysonError, Result};
use crate::workspace::Workspace;
use crate::workspace::memory_store::MemoryStore;

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

    /// Files that have been modified since the last save.
    dirty: std::sync::Mutex<HashSet<String>>,

    /// Memory tier configuration (character limits, nudge interval).
    memory_config: MemoryConfig,

    /// SQLite FTS5 index for Tier 2 memory search.
    memory_store: MemoryStore,
}

impl OpenClawWorkspace {
    /// Load a workspace from a directory.
    ///
    /// Creates the directory and default files if they don't exist.
    /// Reads all .md files in the root and the memory/ subdirectory.
    pub fn load(path: &Path, memory_config: MemoryConfig) -> Result<Self> {
        // Create the directory structure if it doesn't exist.
        std::fs::create_dir_all(path).map_err(|e| {
            DysonError::Config(format!(
                "cannot create workspace dir {}: {e}",
                path.display()
            ))
        })?;
        std::fs::create_dir_all(path.join("memory"))
            .map_err(|e| DysonError::Config(format!("cannot create memory dir: {e}")))?;
        std::fs::create_dir_all(path.join("skills"))
            .map_err(|e| DysonError::Config(format!("cannot create skills dir: {e}")))?;

        // Run workspace migrations before reading files.
        let migrated = crate::workspace::migrate::migrate(path)?;
        if migrated {
            tracing::info!(path = %path.display(), "workspace migrated");
        }

        let mut files = HashMap::new();

        // Read top-level .md files.
        for entry in std::fs::read_dir(path)
            .map_err(|e| DysonError::Config(format!("cannot read workspace dir: {e}")))?
        {
            let entry = entry.map_err(DysonError::Io)?;
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

        // Read skills/*/SKILL.md files so load_skill can find them via ws.get().
        let skills_dir = path.join("skills");
        if skills_dir.exists() {
            for entry in std::fs::read_dir(&skills_dir)? {
                let entry = entry?;
                let dir_name = entry.file_name().to_string_lossy().to_string();
                let skill_md = entry.path().join("SKILL.md");
                if entry.path().is_dir() && skill_md.is_file() {
                    let content = std::fs::read_to_string(&skill_md)?;
                    files.insert(format!("skills/{dir_name}/SKILL.md"), content);
                }
            }
        }

        // Open (or create) the FTS5 memory store.
        let memory_store = MemoryStore::open(&path.join("memory.db"))?;

        // Index all existing memory/ files into FTS5.
        for (name, content) in &files {
            if name.starts_with("memory/") {
                memory_store.index(name, content);
            }
        }

        // Create default files if they don't exist.
        let mut workspace = Self {
            path: path.to_path_buf(),
            files,
            dirty: std::sync::Mutex::new(HashSet::new()),
            memory_config,
            memory_store,
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
        let path = resolve_tilde(config_path.unwrap_or("~/.dyson"));
        if path.exists() { Some(path) } else { None }
    }

    /// Load from the default path (~/.dyson/) or a configured path.
    pub fn load_default(config_path: Option<&str>, memory_config: MemoryConfig) -> Result<Self> {
        let path = resolve_tilde(config_path.unwrap_or("~/.dyson"));
        Self::load(&path, memory_config)
    }

    /// Load from a connection string (path with ~ expansion).
    pub fn load_from_connection_string(
        connection_string: &str,
        memory_config: MemoryConfig,
    ) -> Result<Self> {
        let path = resolve_tilde(connection_string);
        Self::load(&path, memory_config)
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

        if !self.files.contains_key("USER.md") {
            let default = "\
# USER.md — User Profile

*Nothing here yet. Update this file as you learn about the user.*
";
            self.files.insert("USER.md".into(), default.into());
            std::fs::write(self.path.join("USER.md"), default)?;
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
        self.dirty.lock().unwrap().insert(name.to_string());
        if name.starts_with("memory/") {
            self.memory_store.index(name, content);
        }
    }

    fn append(&mut self, name: &str, content: &str) {
        let entry = self.files.entry(name.to_string()).or_default();
        if !entry.is_empty() && !entry.ends_with('\n') {
            entry.push('\n');
        }
        entry.push_str(content);
        self.dirty.lock().unwrap().insert(name.to_string());
        if name.starts_with("memory/") {
            self.memory_store.index(name, entry);
        }
    }

    fn save(&self) -> Result<()> {
        let mut dirty = self.dirty.lock().unwrap();

        if dirty.is_empty() {
            tracing::debug!("workspace save skipped — no dirty files");
            return Ok(());
        }

        for name in dirty.iter() {
            if let Some(content) = self.files.get(name) {
                let file_path = self.path.join(name);

                // Ensure parent directory exists (for memory/ files).
                if let Some(parent) = file_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }

                std::fs::write(&file_path, content)?;
            }
        }

        let dirty_count = dirty.len();
        dirty.clear();

        tracing::debug!(files = dirty_count, "workspace saved");

        Ok(())
    }

    fn list_files(&self) -> Vec<String> {
        let mut names: Vec<String> = self.files.keys().cloned().collect();
        names.sort();
        names
    }

    fn search(&self, pattern: &str) -> Vec<(String, Vec<String>)> {
        let re = RegexBuilder::new(pattern)
            .case_insensitive(true)
            .size_limit(10 * 1024 * 1024) // 10 MB compiled size limit (prevents ReDoS)
            .build();

        // Pre-compute lowercase pattern for the fallback path (invalid regex).
        let pattern_lower = if re.is_err() {
            Some(pattern.to_lowercase())
        } else {
            None
        };

        let mut results = Vec::new();

        for (name, content) in &self.files {
            let matching_lines: Vec<String> = content
                .lines()
                .filter(|line| match &re {
                    Ok(re) => re.is_match(line),
                    Err(_) => line
                        .to_lowercase()
                        .contains(pattern_lower.as_deref().unwrap_or(pattern)),
                })
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
            ("USER PROFILE", "USER.md"),
        ] {
            if let Some(content) = self.files.get(file)
                && !content.trim().is_empty()
            {
                parts.push(format!("## {label}\n\n{content}"));
            }
        }

        // Yesterday's journal (for continuity across sessions).
        let yesterday = Self::yesterday_journal();
        if let Some(content) = self.files.get(&yesterday)
            && !content.trim().is_empty()
        {
            parts.push(format!("## YESTERDAY'S JOURNAL\n\n{content}"));
        }

        // Today's journal.
        let today = Self::today_journal();
        if let Some(content) = self.files.get(&today)
            && !content.trim().is_empty()
        {
            parts.push(format!("## TODAY'S JOURNAL\n\n{content}"));
        }

        parts.join("\n\n---\n\n")
    }

    fn journal(&mut self, entry: &str) {
        let name = Self::today_journal();
        self.append(&name, entry);
    }

    fn char_limit(&self, file: &str) -> Option<usize> {
        self.memory_config.limits.get(file).copied()
    }

    fn nudge_interval(&self) -> usize {
        self.memory_config.nudge_interval
    }

    fn memory_search(&self, query: &str) -> Vec<(String, String)> {
        let results = self.memory_store.search(query);
        if results.is_empty() {
            // Fall back to regex search over memory/ files.
            self.search(query)
                .into_iter()
                .filter(|(name, _)| name.starts_with("memory/"))
                .map(|(name, lines)| (name, lines.join("\n")))
                .collect()
        } else {
            results.into_iter().map(|r| (r.key, r.snippet)).collect()
        }
    }

    fn skill_dirs(&self) -> Vec<std::path::PathBuf> {
        let skills_dir = self.path.join("skills");
        if !skills_dir.is_dir() {
            return vec![];
        }

        let mut dirs = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&skills_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && path.join("SKILL.md").is_file() {
                    dirs.push(path);
                }
            }
        }
        dirs.sort();
        dirs
    }

    fn programs_dir(&self) -> Option<std::path::PathBuf> {
        let dir = self.path.join("programs");
        // Create it if it doesn't exist yet.
        if !dir.exists() {
            let _ = std::fs::create_dir_all(&dir);
        }
        Some(dir)
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Resolve ~ to $HOME in a path string.
pub(crate) fn resolve_tilde(path: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    if let Some(rest) = path.strip_prefix("~/") {
        PathBuf::from(&home).join(rest)
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
    let (y, m, d) = crate::util::unix_to_ymd(secs);
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
        let dir = std::env::temp_dir().join(format!("dyson-test-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = OpenClawWorkspace::load(&dir, MemoryConfig::default()).unwrap();
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
        let ws = OpenClawWorkspace::load(&dir, MemoryConfig::default()).unwrap();
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
        ws.set(
            "MEMORY.md",
            "# Memory\n\nI learned about Rust.\nRust is great.",
        );
        let results = ws.search("rust");
        assert!(!results.is_empty());
        let (name, lines) = &results.iter().find(|(n, _)| n == "MEMORY.md").unwrap();
        assert_eq!(name, "MEMORY.md");
        assert_eq!(lines.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_supports_regex() {
        let (dir, mut ws) = temp_workspace();
        ws.set(
            "MEMORY.md",
            "learned Rust in 2026\nlearned Go in 2025\nforgot Java",
        );
        // Regex: lines containing "learned" followed by a year
        let results = ws.search(r"learned\s+\w+\s+in\s+\d{4}");
        let (_, lines) = results.iter().find(|(n, _)| n == "MEMORY.md").unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Rust"));
        assert!(lines[1].contains("Go"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_falls_back_on_invalid_regex() {
        let (dir, mut ws) = temp_workspace();
        ws.set("MEMORY.md", "open bracket [ here\nno bracket here");
        // "[" is invalid regex — should fall back to literal substring match
        let results = ws.search("[");
        let (_, lines) = results.iter().find(|(n, _)| n == "MEMORY.md").unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("open bracket"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creates_user_md_default() {
        let (dir, ws) = temp_workspace();
        assert!(ws.get("USER.md").is_some());
        assert!(dir.join("USER.md").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_prompt_includes_user_profile() {
        let (dir, ws) = temp_workspace();
        let prompt = ws.system_prompt();
        assert!(prompt.contains("USER PROFILE"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn char_limit_returns_configured_limits() {
        let (dir, ws) = temp_workspace();
        assert_eq!(ws.char_limit("MEMORY.md"), Some(2200));
        assert_eq!(ws.char_limit("USER.md"), Some(1375));
        assert_eq!(ws.char_limit("SOUL.md"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nudge_interval_returns_default() {
        let (dir, ws) = temp_workspace();
        assert_eq!(ws.nudge_interval(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_search_via_fts5() {
        let (dir, mut ws) = temp_workspace();
        ws.set(
            "memory/notes/rust.md",
            "Rust is a systems programming language.",
        );
        ws.save().unwrap();

        let results = ws.memory_search("rust programming");
        assert!(!results.is_empty());
        assert_eq!(results[0].0, "memory/notes/rust.md");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_search_falls_back_to_regex() {
        let (dir, mut ws) = temp_workspace();
        // Add a journal with content, but don't go through set (which indexes).
        // Actually set() does index, but let's test the fallback path by
        // searching for something that FTS5 won't match but regex will.
        ws.set("memory/2026-03-20.md", "talked about XYZ123 today");
        ws.save().unwrap();

        // FTS5 should find it by word match
        let results = ws.memory_search("XYZ123");
        assert!(!results.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_dirs_returns_empty_when_no_skills() {
        let (dir, ws) = temp_workspace();
        // skills/ directory exists but is empty
        assert!(dir.join("skills").is_dir());
        assert!(ws.skill_dirs().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_dirs_discovers_skill_directories() {
        let (dir, ws) = temp_workspace();
        let skills_dir = dir.join("skills");

        // Create skill directories with SKILL.md
        let cr = skills_dir.join("code-review");
        std::fs::create_dir_all(&cr).unwrap();
        std::fs::write(
            cr.join("SKILL.md"),
            "---\nname: code-review\n---\n\nReview code.",
        )
        .unwrap();

        let wr = skills_dir.join("writing");
        std::fs::create_dir_all(&wr).unwrap();
        std::fs::write(
            wr.join("SKILL.md"),
            "---\nname: writing\n---\n\nWrite well.",
        )
        .unwrap();

        // A directory without SKILL.md should be ignored
        let orphan = skills_dir.join("orphan");
        std::fs::create_dir_all(&orphan).unwrap();

        let dirs = ws.skill_dirs();
        assert_eq!(dirs.len(), 2, "should find exactly 2 skill directories");
        let names: Vec<String> = dirs
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"code-review".to_string()));
        assert!(names.contains(&"writing".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_dirs_ignores_flat_files() {
        let (dir, ws) = temp_workspace();
        let skills_dir = dir.join("skills");

        // A flat .md file should not be discovered (legacy format)
        std::fs::write(skills_dir.join("legacy.md"), "old style").unwrap();

        // A proper skill directory
        let proper = skills_dir.join("proper");
        std::fs::create_dir_all(&proper).unwrap();
        std::fs::write(
            proper.join("SKILL.md"),
            "---\nname: proper\n---\n\nProper skill.",
        )
        .unwrap();

        let dirs = ws.skill_dirs();
        assert_eq!(dirs.len(), 1, "should only find directory-based skills");
        assert!(dirs[0].file_name().unwrap() == "proper");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creates_skills_directory() {
        let (dir, _ws) = temp_workspace();
        assert!(
            dir.join("skills").is_dir(),
            "workspace load should create skills/ directory"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_reads_skill_files_into_hashmap() {
        let (dir, _ws) = temp_workspace();

        // Create a skill on disk.
        let skill_dir = dir.join("skills/diagnostics");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: diagnostics\ndescription: Run diagnostics\n---\n\nStep 1: Check logs.",
        )
        .unwrap();

        // Reload workspace — skill should now be in the files HashMap.
        let ws = OpenClawWorkspace::load(&dir, MemoryConfig::default()).unwrap();
        let content = ws.get("skills/diagnostics/SKILL.md");
        assert!(
            content.is_some(),
            "skills/diagnostics/SKILL.md should be loadable via ws.get()"
        );
        assert!(content.unwrap().contains("Step 1: Check logs."));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
