// ===========================================================================
// Persistence — agent memory, identity, and conversation history.
//
// LEARNING OVERVIEW
//
// What this module does:
//   Manages everything the agent remembers across sessions.  Split into
//   two concerns:
//
//   workspace.rs   — identity and memory (SOUL.md, MEMORY.md, journals)
//   chat_store.rs  — per-chat conversation history (save/load/rotate)
//
// Why two files?
//   Workspace and ChatStore serve different purposes:
//   - Workspace is the agent's long-term identity and memory, shared
//     across all conversations.  It's loaded once on startup and
//     occasionally updated.
//   - ChatStore is per-chat conversation history, used by controllers
//     (like Telegram) to persist and restore individual conversations.
//
//   Splitting them makes each file focused and easy to read.
// ===========================================================================

pub mod chat_store;
pub mod workspace;

// Re-export Workspace at the module level for convenience.
// This lets callers write `persistence::Workspace` instead of
// `persistence::workspace::Workspace`.
pub use workspace::Workspace;
