// ===========================================================================
// InMemoryChatHistory — in-memory chat history for testing.
//
// No filesystem access.  All operations work on a Mutex-protected HashMap.
// Rotation is simulated by moving entries to a separate "rotated" map.
// ===========================================================================

use std::collections::HashMap;
use std::sync::Mutex;

use crate::chat_history::ChatHistory;
use crate::error::Result;
use crate::message::Message;

/// In-memory chat history — no filesystem, no persistence.
pub struct InMemoryChatHistory {
    data: Mutex<HashMap<String, Vec<Message>>>,
}

impl InMemoryChatHistory {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryChatHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatHistory for InMemoryChatHistory {
    fn save(&self, chat_id: &str, messages: &[Message]) -> Result<()> {
        self.data
            .lock()
            .unwrap()
            .insert(chat_id.to_string(), messages.to_vec());
        Ok(())
    }

    fn load(&self, chat_id: &str) -> Result<Vec<Message>> {
        Ok(self
            .data
            .lock()
            .unwrap()
            .get(chat_id)
            .cloned()
            .unwrap_or_default())
    }

    fn rotate(&self, chat_id: &str) -> Result<()> {
        self.data.lock().unwrap().remove(chat_id);
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

    #[test]
    fn save_and_load() {
        let store = InMemoryChatHistory::new();
        let messages = vec![Message::user("hello")];
        store.save("chat_1", &messages).unwrap();

        let loaded = store.load("chat_1").unwrap();
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn load_nonexistent() {
        let store = InMemoryChatHistory::new();
        let loaded = store.load("nonexistent").unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn rotate_clears() {
        let store = InMemoryChatHistory::new();
        store.save("chat_1", &[Message::user("hi")]).unwrap();
        store.rotate("chat_1").unwrap();
        assert!(store.load("chat_1").unwrap().is_empty());
    }
}
