// ===========================================================================
// InMemoryWorkspace — in-memory workspace for testing.
//
// No filesystem access.  All operations work on an in-memory HashMap.
// save() is a no-op.  Useful for unit tests that don't need persistence.
// ===========================================================================

use std::collections::HashMap;

use regex::RegexBuilder;

use crate::error::Result;
use crate::workspace::Workspace;
use crate::workspace::openclaw::chrono_today;

/// In-memory workspace — no filesystem, no persistence.
pub struct InMemoryWorkspace {
    files: HashMap<String, String>,
    limits: HashMap<String, usize>,
    nudge_interval: usize,
}

impl InMemoryWorkspace {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
            limits: HashMap::new(),
            nudge_interval: 5,
        }
    }

    /// Builder: add a file to the workspace.
    pub fn with_file(mut self, name: &str, content: &str) -> Self {
        self.files.insert(name.to_string(), content.to_string());
        self
    }

    /// Builder: set a character limit for a file.
    pub fn with_limit(mut self, file: &str, max_chars: usize) -> Self {
        self.limits.insert(file.to_string(), max_chars);
        self
    }
}

impl Default for InMemoryWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace for InMemoryWorkspace {
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
        Ok(()) // no-op
    }

    fn list_files(&self) -> Vec<String> {
        let mut names: Vec<String> = self.files.keys().cloned().collect();
        names.sort();
        names
    }

    fn search(&self, pattern: &str) -> Vec<(String, Vec<String>)> {
        let re = RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build();

        let mut results = Vec::new();

        for (name, content) in &self.files {
            let matching_lines: Vec<String> = content
                .lines()
                .filter(|line| match &re {
                    Ok(re) => re.is_match(line),
                    Err(_) => line.to_lowercase().contains(&pattern.to_lowercase()),
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

        for (label, file) in [
            ("PERSONALITY", "SOUL.md"),
            ("IDENTITY", "IDENTITY.md"),
            ("LONG-TERM MEMORY", "MEMORY.md"),
            ("USER PROFILE", "USER.md"),
        ] {
            if let Some(content) = self.files.get(file) {
                if !content.trim().is_empty() {
                    parts.push(format!("## {label}\n\n{content}"));
                }
            }
        }

        parts.join("\n\n---\n\n")
    }

    fn journal(&mut self, entry: &str) {
        let today = chrono_today();
        let name = format!("memory/{today}.md");
        self.append(&name, entry);
    }

    fn char_limit(&self, file: &str) -> Option<usize> {
        self.limits.get(file).copied()
    }

    fn nudge_interval(&self) -> usize {
        self.nudge_interval
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_operations() {
        let mut ws = InMemoryWorkspace::new()
            .with_file("SOUL.md", "Be helpful.");

        assert_eq!(ws.get("SOUL.md").unwrap(), "Be helpful.");
        assert!(ws.get("nonexistent").is_none());

        ws.set("MEMORY.md", "Remember this.");
        assert_eq!(ws.get("MEMORY.md").unwrap(), "Remember this.");

        ws.append("MEMORY.md", "And this too.");
        let content = ws.get("MEMORY.md").unwrap();
        assert!(content.contains("Remember this."));
        assert!(content.contains("And this too."));
    }

    #[test]
    fn list_and_search() {
        let ws = InMemoryWorkspace::new()
            .with_file("SOUL.md", "Be kind and helpful.")
            .with_file("MEMORY.md", "Learned about Rust.");

        let files = ws.list_files();
        assert_eq!(files.len(), 2);

        let results = ws.search("rust");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "MEMORY.md");
    }

    #[test]
    fn search_supports_regex() {
        let ws = InMemoryWorkspace::new()
            .with_file("MEMORY.md", "learned Rust in 2026\nlearned Go in 2025\nforgot Java");

        let results = ws.search(r"learned\s+\w+\s+in\s+\d{4}");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.len(), 2);
    }

    #[test]
    fn search_falls_back_on_invalid_regex() {
        let ws = InMemoryWorkspace::new()
            .with_file("MEMORY.md", "open bracket [ here\nno bracket here");

        let results = ws.search("[");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.len(), 1);
        assert!(results[0].1[0].contains("open bracket"));
    }

    #[test]
    fn save_is_noop() {
        let ws = InMemoryWorkspace::new();
        assert!(ws.save().is_ok());
    }

    #[test]
    fn with_limit_sets_char_limit() {
        let ws = InMemoryWorkspace::new()
            .with_limit("MEMORY.md", 100)
            .with_limit("USER.md", 50);

        assert_eq!(ws.char_limit("MEMORY.md"), Some(100));
        assert_eq!(ws.char_limit("USER.md"), Some(50));
        assert_eq!(ws.char_limit("SOUL.md"), None);
    }

    #[test]
    fn system_prompt_includes_user_profile() {
        let ws = InMemoryWorkspace::new()
            .with_file("USER.md", "User likes Rust.");

        let prompt = ws.system_prompt();
        assert!(prompt.contains("USER PROFILE"));
        assert!(prompt.contains("User likes Rust"));
    }
}
