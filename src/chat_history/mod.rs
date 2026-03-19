// ===========================================================================
// ChatHistory — trait for persisting per-chat conversation history.
//
// LEARNING OVERVIEW
//
// What this module does:
//   Defines the ChatHistory trait and its implementations.  Chat history
//   is per-conversation state — each chat gets its own history that can
//   be saved, loaded, and rotated (archived on /clear).
//
// Module layout:
//   mod.rs      — ChatHistory trait + factory (this file)
//   disk.rs     — DiskChatHistory (JSON files on disk)
//   in_memory.rs — InMemoryChatHistory (for testing)
//
// How it differs from Workspace:
//   - Workspace: agent's long-term identity and memory, shared across
//     all conversations.  Loaded once on startup.
//   - ChatHistory: per-chat conversation messages, used by controllers
//     to persist and restore individual conversations.
//
// Configuration:
//   In dyson.json:
//   ```json
//   {
//     "chat_history": {
//       "backend": "disk",
//       "connection_string": "~/.dyson/chats"
//     }
//   }
//   ```
// ===========================================================================

pub mod disk;
pub mod in_memory;

pub use disk::DiskChatHistory;
pub use in_memory::InMemoryChatHistory;

use crate::config::ChatHistoryConfig;
use crate::error::{DysonError, Result};
use crate::message::Message;

// ---------------------------------------------------------------------------
// ChatHistory trait
// ---------------------------------------------------------------------------

/// Persistent storage for per-chat conversation history.
///
/// Implementors decide where and how messages are stored.  Controllers
/// call `save()` after each agent turn and `load()` when creating an
/// agent for a chat.
pub trait ChatHistory: Send + Sync {
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
// Factory
// ---------------------------------------------------------------------------

/// Create a chat history backend from configuration.
///
/// Dispatches on `config.backend` to construct the appropriate implementation.
pub fn create_chat_history(config: &ChatHistoryConfig) -> Result<Box<dyn ChatHistory>> {
    match config.backend.as_str() {
        "disk" => {
            let store = DiskChatHistory::new_from_connection_string(&config.connection_string)?;
            Ok(Box::new(store))
        }
        other => Err(DysonError::Config(format!(
            "unknown chat_history backend: '{other}'.  Supported: 'disk'."
        ))),
    }
}
