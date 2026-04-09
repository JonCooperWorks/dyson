// ===========================================================================
// ChannelWorkspace — wrapper that protects identity files from writes.
//
// Wraps any Workspace implementation and silently drops writes to a set
// of protected keys (e.g. SOUL.md, IDENTITY.md).  Reads delegate straight
// through to the inner workspace.
//
// Used by public agents: each channel gets its own workspace directory
// with identity files symlinked from the operator's main workspace.
// The wrapper ensures those symlinks are never written through.
// ===========================================================================

use std::collections::HashSet;

use crate::error::Result;
use crate::workspace::Workspace;

/// A workspace wrapper that makes designated keys read-only.
///
/// All trait methods delegate to the inner workspace except `set` and
/// `append`, which silently skip writes to protected keys.
pub struct ChannelWorkspace {
    inner: Box<dyn Workspace>,
    protected: HashSet<String>,
}

impl ChannelWorkspace {
    pub fn new(inner: Box<dyn Workspace>, protected: impl IntoIterator<Item = String>) -> Self {
        Self {
            inner,
            protected: protected.into_iter().collect(),
        }
    }
}

impl Workspace for ChannelWorkspace {
    fn get(&self, name: &str) -> Option<String> {
        self.inner.get(name)
    }

    fn set(&mut self, name: &str, content: &str) {
        if !self.protected.contains(name) {
            self.inner.set(name, content);
        }
    }

    fn append(&mut self, name: &str, content: &str) {
        if !self.protected.contains(name) {
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
    fn protected_key_write_is_dropped() {
        let inner = InMemoryWorkspace::new()
            .with_file("SOUL.md", "Be helpful.");
        let mut ws = ChannelWorkspace::new(
            Box::new(inner),
            ["SOUL.md".to_string()],
        );

        ws.set("SOUL.md", "Be evil.");
        assert_eq!(ws.get("SOUL.md").unwrap(), "Be helpful.");
    }

    #[test]
    fn protected_key_append_is_dropped() {
        let inner = InMemoryWorkspace::new()
            .with_file("SOUL.md", "Be helpful.");
        let mut ws = ChannelWorkspace::new(
            Box::new(inner),
            ["SOUL.md".to_string()],
        );

        ws.append("SOUL.md", " And evil.");
        assert_eq!(ws.get("SOUL.md").unwrap(), "Be helpful.");
    }

    #[test]
    fn unprotected_key_write_succeeds() {
        let inner = InMemoryWorkspace::new()
            .with_file("MEMORY.md", "old");
        let mut ws = ChannelWorkspace::new(
            Box::new(inner),
            ["SOUL.md".to_string()],
        );

        ws.set("MEMORY.md", "new");
        assert_eq!(ws.get("MEMORY.md").unwrap(), "new");
    }

    #[test]
    fn skill_dirs_returns_empty() {
        let inner = InMemoryWorkspace::new();
        let ws = ChannelWorkspace::new(Box::new(inner), Vec::<String>::new());
        assert!(ws.skill_dirs().is_empty());
    }

    #[test]
    fn programs_dir_returns_none() {
        let inner = InMemoryWorkspace::new();
        let ws = ChannelWorkspace::new(Box::new(inner), Vec::<String>::new());
        assert!(ws.programs_dir().is_none());
    }
}
