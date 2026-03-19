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
    /// Returns an empty Vec if no history exists for this chat_id.
    fn load(&self, chat_id: &str) -> Result<Vec<Message>>;

    /// Delete the conversation history for a chat.
    ///
    /// Called on /clear to remove persisted state.
    fn delete(&self, chat_id: &str) -> Result<()>;
}

// ---------------------------------------------------------------------------
// JsonChatStore — one JSON file per chat
// ---------------------------------------------------------------------------

/// File-based chat store: one JSON file per chat in a directory.
///
/// ```text
/// ~/.dyson/chats/
///   2102424765.json    ← chat history for Telegram chat 2102424765
///   9876543210.json
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

    fn delete(&self, chat_id: &str) -> Result<()> {
        let path = self.chat_path(chat_id);
        if path.exists() {
            std::fs::remove_file(&path)?;
            tracing::debug!(chat_id = chat_id, "chat history deleted");
        }
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
        let dir = std::env::temp_dir().join(format!("dyson_chat_test_{}_{}", name, std::process::id()));
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
    fn delete_removes_file() {
        let (dir, store) = temp_store("delete");

        store.save("chat_2", &[Message::user("test")]).unwrap();
        assert!(!store.load("chat_2").unwrap().is_empty());

        store.delete("chat_2").unwrap();
        assert!(store.load("chat_2").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_nonexistent_is_ok() {
        let (dir, store) = temp_store("delete_none");
        store.delete("nonexistent").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
