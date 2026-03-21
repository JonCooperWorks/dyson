# Chat Persistence

Dyson persists per-chat conversation history so context survives restarts.
The Telegram controller maintains one agent per chat with full message
history. History is saved to disk after each turn and restored on startup.

**Key files:**
- `src/chat_history/mod.rs` — `ChatHistory` trait
- `src/chat_history/disk.rs` — `DiskChatHistory` (JSON-file-per-chat persistence)
- `src/chat_history/in_memory.rs` — `InMemoryChatHistory` (for testing)
- `src/controller/telegram.rs` — per-chat agent management, `/clear` and `/memory` commands
- `src/agent/mod.rs` — `Agent::messages()`, `Agent::set_messages()`, `Agent::clear()`

---

## ChatHistory Trait

```rust
pub trait ChatHistory: Send + Sync {
    fn save(&self, chat_id: &str, messages: &[Message]) -> Result<()>;
    fn load(&self, chat_id: &str) -> Result<Vec<Message>>;
    fn rotate(&self, chat_id: &str) -> Result<()>;
}
```

The trait is intentionally minimal so you can swap backends:

| Backend | Use case |
|---------|----------|
| `DiskChatHistory` (default) | One JSON file per chat in `~/.dyson/chats/` |
| Database (Postgres, SQLite) | Multi-server deployments, query history |
| RAG pipeline | Index and retrieve relevant past context |
| `InMemoryChatHistory` | Testing, ephemeral sessions |

`rotate()` archives the current conversation (preserves the file with a
timestamp suffix) and starts fresh — used by `/clear`.  Old history files
are preserved for review or future RAG indexing.

---

## DiskChatHistory

The default implementation stores one JSON file per chat:

```
~/.dyson/chats/
  2102424765.json    <- chat history for Telegram chat 2102424765
  9876543210.json
```

Each file contains a JSON array of `Message` objects with the full
conversation history. Files are human-readable and easy to back up.

---

## Telegram Commands

| Command | Effect |
|---------|--------|
| `/clear` | Delete conversation history (in-memory + on disk), start fresh |
| `/memory <note>` | Append a timestamped note to `MEMORY.md` in the workspace |
| `/whoami` | Reply with the chat ID (no LLM call) |

---

## How It Works

1. **First message from a chat**: Agent is created, disk history is loaded
   (if any), and `agent.set_messages()` restores the conversation.

2. **Each message**: The per-chat agent runs with full context from previous
   turns. After the agent responds, `chat_store.save()` writes the updated
   history to disk.

3. **`/clear`**: Removes the in-memory agent and calls `chat_store.delete()`
   to remove the on-disk history. The next message starts a fresh conversation.

4. **Config reload**: When `dyson.json` or workspace files change, all
   in-memory agents are cleared (they'll be recreated with the new config
   on the next message). On-disk history is preserved and restored.

5. **Service restart**: On startup, no agents exist in memory. When a chat
   sends a message, the agent is created and history is loaded from disk
   automatically.

---

See also: [Agent Loop](agent-loop.md) ·
[Architecture Overview](architecture-overview.md) ·
[Configuration](configuration.md)
