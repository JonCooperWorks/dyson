// ===========================================================================
// DiskChatHistory — JSON files on disk, one per chat.
//
// Active conversations use `{chat_id}.json`.  When rotated, the file
// is renamed to `{chat_id}.{timestamp}.json` and a fresh file is
// created on the next save.
//
// ```text
// ~/.dyson/chats/
//   2102424765.json                      ← current conversation
//   2102424765.2026-03-19T14-30-00.json  ← rotated (old) conversation
//   2102424765.2026-03-18T09-15-22.json  ← even older
// ```
// ===========================================================================

use std::path::PathBuf;

use crate::chat_history::ChatHistory;
use crate::error::Result;
use crate::message::Message;
use crate::workspace::openclaw::resolve_tilde;

// ---------------------------------------------------------------------------
// DiskChatHistory
// ---------------------------------------------------------------------------

/// File-based chat store: one JSON file per chat in a directory.
pub struct DiskChatHistory {
    /// Directory where chat JSON files are stored.
    dir: PathBuf,
}

impl DiskChatHistory {
    /// Create a new disk chat history in the given directory.
    ///
    /// Creates the directory if it doesn't exist.
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Create from a connection string (path with ~ expansion).
    pub fn new_from_connection_string(connection_string: &str) -> Result<Self> {
        let path = resolve_tilde(connection_string);
        Self::new(path)
    }

    fn chat_path(&self, chat_id: &str) -> PathBuf {
        self.dir.join(format!("{chat_id}.json"))
    }

    /// Generate a timestamp string for rotation filenames.
    fn rotation_timestamp() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let (y, m, d) = crate::util::unix_to_ymd(now);

        let day_secs = now % 86400;
        let h = day_secs / 3600;
        let min = (day_secs % 3600) / 60;
        let s = day_secs % 60;

        format!("{y:04}-{m:02}-{d:02}T{h:02}-{min:02}-{s:02}")
    }
}

impl ChatHistory for DiskChatHistory {
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

    fn temp_store(name: &str) -> (PathBuf, DiskChatHistory) {
        let dir = std::env::temp_dir()
            .join(format!("dyson_chat_test_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = DiskChatHistory::new(dir.clone()).unwrap();
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

        store.save("chat_1", &[Message::user("old message")]).unwrap();
        assert!(!store.load("chat_1").unwrap().is_empty());

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

        store.save("chat_1", &[Message::user("first")]).unwrap();
        store.rotate("chat_1").unwrap();
        store.save("chat_1", &[Message::user("second")]).unwrap();

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
