// ===========================================================================
// ChannelWorkspace — write-restricted wrapper for public agents.
//
// Wraps any Workspace implementation and only allows writes to an
// explicit set of writable keys.  Everything else is silently dropped.
// This is a whitelist model: new files are protected by default.
//
// Used by public agents: each channel gets its own workspace directory
// with identity files symlinked from the operator's main workspace.
// The wrapper ensures only memory-related keys can be written.
// ===========================================================================

use std::collections::HashSet;

use crate::error::Result;
use crate::workspace::Workspace;

/// Workspace wrapper that only allows writes to explicitly listed keys.
///
/// Reads delegate straight through.  `set` and `append` only forward to
/// the inner workspace when the key is in `writable` or matches a writable
/// prefix (e.g. `"memory/"` allows all journal writes).
pub struct ChannelWorkspace {
    inner: Box<dyn Workspace>,
    writable: HashSet<String>,
    writable_prefixes: Vec<String>,
}

impl ChannelWorkspace {
    pub fn new(inner: Box<dyn Workspace>) -> Self {
        Self {
            inner,
            writable: HashSet::new(),
            writable_prefixes: Vec::new(),
        }
    }

    /// Allow writes to an exact key (e.g. `"MEMORY.md"`).
    pub fn allow(mut self, key: &str) -> Self {
        self.writable.insert(key.to_string());
        self
    }

    /// Allow writes to any key starting with this prefix (e.g. `"memory/"`).
    pub fn allow_prefix(mut self, prefix: &str) -> Self {
        self.writable_prefixes.push(prefix.to_string());
        self
    }

    fn can_write(&self, name: &str) -> bool {
        self.writable.contains(name)
            || self.writable_prefixes.iter().any(|p| name.starts_with(p.as_str()))
    }
}

impl Workspace for ChannelWorkspace {
    fn get(&self, name: &str) -> Option<String> {
        self.inner.get(name)
    }

    fn set(&mut self, name: &str, content: &str) {
        if self.can_write(name) {
            self.inner.set(name, content);
        }
    }

    fn append(&mut self, name: &str, content: &str) {
        if self.can_write(name) {
            self.inner.append(name, content);
        }
    }

    fn save(&self) -> Result<()> {
        self.inner.save()
    }

    fn list_files(&self) -> Vec<String> {
        self.inner.list_files()
    }

    fn search(&self, pattern: &str) -> Vec<(String, Vec<String>)> {
        self.inner.search(pattern)
    }

    fn system_prompt(&self) -> String {
        self.inner.system_prompt()
    }

    fn journal(&mut self, entry: &str) {
        // Journals write to memory/YYYY-MM-DD.md — allowed by the
        // "memory/" prefix, so delegate directly.
        self.inner.journal(entry);
    }

    fn char_limit(&self, file: &str) -> Option<usize> {
        self.inner.char_limit(file)
    }

    fn nudge_interval(&self) -> usize {
        self.inner.nudge_interval()
    }

    fn memory_search(&self, query: &str) -> Vec<(String, String)> {
        self.inner.memory_search(query)
    }

    fn skill_dirs(&self) -> Vec<std::path::PathBuf> {
        // Public agents don't load skills from the workspace.
        vec![]
    }

    fn programs_dir(&self) -> Option<std::path::PathBuf> {
        // Public agents don't get a programs directory.
        None
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::InMemoryWorkspace;

    #[test]
    fn unlisted_key_write_is_dropped() {
        let inner = InMemoryWorkspace::new()
            .with_file("SOUL.md", "Be helpful.");
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md");

        ws.set("SOUL.md", "Be evil.");
        assert_eq!(ws.get("SOUL.md").unwrap(), "Be helpful.");
    }

    #[test]
    fn unlisted_key_append_is_dropped() {
        let inner = InMemoryWorkspace::new()
            .with_file("SOUL.md", "Be helpful.");
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md");

        ws.append("SOUL.md", " And evil.");
        assert_eq!(ws.get("SOUL.md").unwrap(), "Be helpful.");
    }

    #[test]
    fn allowed_key_write_succeeds() {
        let inner = InMemoryWorkspace::new()
            .with_file("MEMORY.md", "old");
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md");

        ws.set("MEMORY.md", "new");
        assert_eq!(ws.get("MEMORY.md").unwrap(), "new");
    }

    #[test]
    fn prefix_allows_nested_writes() {
        let inner = InMemoryWorkspace::new();
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow_prefix("memory/");

        ws.set("memory/2026-04-09.md", "journal entry");
        assert_eq!(ws.get("memory/2026-04-09.md").unwrap(), "journal entry");
    }

    #[test]
    fn prefix_does_not_allow_exact_match() {
        let inner = InMemoryWorkspace::new()
            .with_file("MEMORY.md", "original");
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow_prefix("memory/");

        // "memory/" prefix does not match "MEMORY.md"
        ws.set("MEMORY.md", "overwrite");
        assert_eq!(ws.get("MEMORY.md").unwrap(), "original");
    }

    #[test]
    fn new_unknown_file_is_protected_by_default() {
        let inner = InMemoryWorkspace::new();
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md");

        ws.set("EVIL.md", "bad content");
        assert!(ws.get("EVIL.md").is_none());
    }

    #[test]
    fn skill_dirs_returns_empty() {
        let inner = InMemoryWorkspace::new();
        let ws = ChannelWorkspace::new(Box::new(inner));
        assert!(ws.skill_dirs().is_empty());
    }

    #[test]
    fn programs_dir_returns_none() {
        let inner = InMemoryWorkspace::new();
        let ws = ChannelWorkspace::new(Box::new(inner));
        assert!(ws.programs_dir().is_none());
    }
}
