// ===========================================================================
// ChatStore — trait for persisting per-chat conversation history.
//
// The trait is intentionally simple: save and load a list of messages
// keyed by a chat ID string.  This lets you swap in any backend:
//
// - JsonChatStore (default) — one JSON file per chat in a directory
// - A database (Postgres, SQLite, Redis, etc.)
// - A RAG pipeline that indexes and retrieves relevant past messages
// - An in-memory store for testing
//
// The agent doesn't know or care which backend is in use.
//
// Chat rotation:
//   When the user clears context, the current chat file is rotated
//   (renamed with a timestamp) rather than deleted.  This preserves
//   history for review or RAG indexing.  The agent always picks up
//   the newest (unrotated) file.
// ===========================================================================

use crate::error::Result;
use crate::message::Message;

// ---------------------------------------------------------------------------
// ChatStore trait
// ---------------------------------------------------------------------------

/// Persistent storage for per-chat conversation history.
///
/// Implementors decide where and how messages are stored.  The Telegram
/// controller (or any controller) calls `save()` after each agent turn
/// and `load()` when creating an agent for a chat.
pub trait ChatStore: Send + Sync {
    /// Save the conversation history for a chat.
    ///
    /// Called after each agent turn.  Replaces any previously saved
    /// history for this chat_id.
    fn save(&self, chat_id: &str, messages: &[Message]) -> Result<()>;

    /// Load the conversation history for a chat.
    ///
    /// Returns the newest (current) conversation.  Returns an empty Vec
    /// if no history exists for this chat_id.
    fn load(&self, chat_id: &str) -> Result<Vec<Message>>;

    /// Rotate the conversation history for a chat.
    ///
    /// Called on /clear.  The current history is archived (not deleted)
    /// and a fresh conversation starts.  Old history files are preserved
    /// for review or RAG indexing.
    fn rotate(&self, chat_id: &str) -> Result<()>;
}

// ---------------------------------------------------------------------------
// JsonChatStore — one JSON file per chat, with rotation
// ---------------------------------------------------------------------------

/// File-based chat store: one JSON file per chat in a directory.
///
/// Active conversations use `{chat_id}.json`.  When rotated, the file
/// is renamed to `{chat_id}.{timestamp}.json` and a fresh file is
/// created on the next save.
///
/// ```text
/// ~/.dyson/chats/
///   2102424765.json                  ← current conversation
///   2102424765.2026-03-19T14-30-00.json  ← rotated (old) conversation
///   2102424765.2026-03-18T09-15-22.json  ← even older
/// ```
///
/// Simple, zero-dependency, easy to inspect and back up.
pub struct JsonChatStore {
    /// Directory where chat JSON files are stored.
    dir: std::path::PathBuf,
}

impl JsonChatStore {
    /// Create a new JSON chat store in the given directory.
    ///
    /// Creates the directory if it doesn't exist.
    pub fn new(dir: std::path::PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn chat_path(&self, chat_id: &str) -> std::path::PathBuf {
        self.dir.join(format!("{chat_id}.json"))
    }

    /// Generate a timestamp string for rotation filenames.
    fn rotation_timestamp() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Convert to readable timestamp using the same algorithm as chrono_today.
        let z = (now / 86400) as i64 + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = (z - era * 146097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };

        let day_secs = now % 86400;
        let h = day_secs / 3600;
        let min = (day_secs % 3600) / 60;
        let s = day_secs % 60;

        format!("{y:04}-{m:02}-{d:02}T{h:02}-{min:02}-{s:02}")
    }
}

impl ChatStore for JsonChatStore {
    fn save(&self, chat_id: &str, messages: &[Message]) -> Result<()> {
        let path = self.chat_path(chat_id);
        let json = serde_json::to_string_pretty(messages)?;
        std::fs::write(&path, json)?;
        tracing::debug!(chat_id = chat_id, path = %path.display(), "chat history saved");
        Ok(())
    }

    fn load(&self, chat_id: &str) -> Result<Vec<Message>> {
        let path = self.chat_path(chat_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path)?;
        let messages: Vec<Message> = serde_json::from_str(&content)?;
        tracing::debug!(
            chat_id = chat_id,
            messages = messages.len(),
            "chat history loaded"
        );
        Ok(messages)
    }

    fn rotate(&self, chat_id: &str) -> Result<()> {
        let path = self.chat_path(chat_id);
        if !path.exists() {
            return Ok(());
        }

        let timestamp = Self::rotation_timestamp();
        let rotated = self.dir.join(format!("{chat_id}.{timestamp}.json"));
        std::fs::rename(&path, &rotated)?;
        tracing::info!(
            chat_id = chat_id,
            rotated = %rotated.display(),
            "chat history rotated"
        );
        Ok(())
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;

    fn temp_store(name: &str) -> (std::path::PathBuf, JsonChatStore) {
        let dir = std::env::temp_dir()
            .join(format!("dyson_chat_test_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = JsonChatStore::new(dir.clone()).unwrap();
        (dir, store)
    }

    #[test]
    fn save_and_load() {
        let (dir, store) = temp_store("save_load");

        let messages = vec![
            Message::user("hello"),
            Message::assistant(vec![crate::message::ContentBlock::Text {
                text: "hi there".into(),
            }]),
        ];

        store.save("chat_1", &messages).unwrap();
        let loaded = store.load("chat_1").unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].role, crate::message::Role::User);
        assert_eq!(loaded[1].role, crate::message::Role::Assistant);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_nonexistent_returns_empty() {
        let (dir, store) = temp_store("load_none");
        let loaded = store.load("nonexistent").unwrap();
        assert!(loaded.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotate_preserves_old_and_clears_current() {
        let (dir, store) = temp_store("rotate");

        // Save a conversation.
        store.save("chat_1", &[Message::user("old message")]).unwrap();
        assert!(!store.load("chat_1").unwrap().is_empty());

        // Rotate — current file should be gone, but a timestamped copy exists.
        store.rotate("chat_1").unwrap();

        // Current chat should be empty (file renamed).
        assert!(store.load("chat_1").unwrap().is_empty());

        // A rotated file should exist in the directory.
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("chat_1.")
            })
            .collect();
        assert_eq!(files.len(), 1);

        // The rotated file should contain the old messages.
        let rotated_content = std::fs::read_to_string(files[0].path()).unwrap();
        let rotated_msgs: Vec<Message> = serde_json::from_str(&rotated_content).unwrap();
        assert_eq!(rotated_msgs.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotate_nonexistent_is_ok() {
        let (dir, store) = temp_store("rotate_none");
        store.rotate("nonexistent").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_save_after_rotate() {
        let (dir, store) = temp_store("save_after_rotate");

        // Save, rotate, save again.
        store.save("chat_1", &[Message::user("first")]).unwrap();
        store.rotate("chat_1").unwrap();
        store.save("chat_1", &[Message::user("second")]).unwrap();

        // Load should return the new conversation.
        let loaded = store.load("chat_1").unwrap();
        assert_eq!(loaded.len(), 1);
        match &loaded[0].content[0] {
            crate::message::ContentBlock::Text { text } => assert_eq!(text, "second"),
            other => panic!("expected Text, got: {other:?}"),
        }

        // Two files: current + one rotated.
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("chat_1")
            })
            .collect();
        assert_eq!(files.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
