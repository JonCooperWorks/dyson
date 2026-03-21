// ===========================================================================
// MemoryStore — SQLite FTS5 full-text search over workspace memory files.
//
// Provides Tier 2 memory: content that overflows Tier 1 files (MEMORY.md,
// USER.md) can be stored in memory/notes/ and searched via FTS5.
//
// Schema:
//   CREATE VIRTUAL TABLE memory_fts USING fts5(key, content)
//
// The `key` column is the workspace file name (e.g. "memory/notes/rust.md").
// The `content` column is the full file text.
// ===========================================================================

use std::path::Path;
use std::sync::Mutex;

use crate::error::{DysonError, Result};

/// A search result from the FTS5 index.
pub struct SearchResult {
    /// The workspace file key (e.g. "memory/notes/rust.md").
    pub key: String,
    /// A text snippet with search term highlights.
    pub snippet: String,
}

/// SQLite FTS5 wrapper for full-text search over memory files.
///
/// Wrapped in a Mutex because `rusqlite::Connection` is not `Sync`
/// (it uses internal `RefCell`).  The workspace trait requires `Sync`.
pub struct MemoryStore {
    conn: Mutex<rusqlite::Connection>,
}

impl MemoryStore {
    /// Open (or create) the FTS5 database at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = rusqlite::Connection::open(path).map_err(|e| {
            DysonError::Config(format!("cannot open memory store at {}: {e}", path.display()))
        })?;

        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(key, content);",
        )
        .map_err(|e| DysonError::Config(format!("cannot create FTS5 table: {e}")))?;

        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Open an in-memory database (for testing).
    pub fn open_in_memory() -> Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()
            .map_err(|e| DysonError::Config(format!("cannot open in-memory store: {e}")))?;

        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(key, content);",
        )
        .map_err(|e| DysonError::Config(format!("cannot create FTS5 table: {e}")))?;

        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Insert or update a file in the FTS5 index.
    pub fn index(&self, key: &str, content: &str) {
        let conn = self.conn.lock().unwrap();
        // Delete existing entry first (upsert pattern for FTS5).
        let _ = conn.execute(
            "DELETE FROM memory_fts WHERE key = ?1",
            rusqlite::params![key],
        );
        let _ = conn.execute(
            "INSERT INTO memory_fts (key, content) VALUES (?1, ?2)",
            rusqlite::params![key, content],
        );
    }

    /// Search the FTS5 index.  Returns matching files with snippet highlights.
    pub fn search(&self, query: &str) -> Vec<SearchResult> {
        // Sanitize query for FTS5: wrap each word in quotes to avoid syntax errors.
        let safe_query: String = query
            .split_whitespace()
            .map(|w| format!("\"{}\"", w.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" ");

        if safe_query.is_empty() {
            return vec![];
        }

        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT key, snippet(memory_fts, 1, '**', '**', '...', 64) \
             FROM memory_fts WHERE memory_fts MATCH ?1 \
             ORDER BY rank LIMIT 20",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };

        stmt.query_map(rusqlite::params![safe_query], |row| {
            Ok(SearchResult {
                key: row.get(0)?,
                snippet: row.get(1)?,
            })
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// Remove a file from the FTS5 index.
    pub fn remove(&self, key: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "DELETE FROM memory_fts WHERE key = ?1",
            rusqlite::params![key],
        );
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_and_search() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.index("memory/notes/rust.md", "Rust is a systems programming language focused on safety.");
        store.index("memory/notes/go.md", "Go is a statically typed language designed at Google.");

        let results = store.search("rust safety");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "memory/notes/rust.md");
        assert!(results[0].snippet.contains("Rust"));
    }

    #[test]
    fn search_no_results() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.index("memory/notes/rust.md", "Rust is great.");

        let results = store.search("python");
        assert!(results.is_empty());
    }

    #[test]
    fn upsert_replaces_content() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.index("memory/notes/test.md", "old content about cats");
        store.index("memory/notes/test.md", "new content about dogs");

        let results = store.search("cats");
        assert!(results.is_empty());

        let results = store.search("dogs");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn remove_deletes_entry() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.index("memory/notes/test.md", "important data");

        store.remove("memory/notes/test.md");

        let results = store.search("important");
        assert!(results.is_empty());
    }

    #[test]
    fn empty_query_returns_nothing() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.index("test.md", "some content");

        let results = store.search("");
        assert!(results.is_empty());
    }

    #[test]
    fn special_characters_in_query() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.index("test.md", "user said hello world");

        // Queries with special chars should not crash.
        let results = store.search("hello \"world");
        assert!(!results.is_empty());
    }
}
