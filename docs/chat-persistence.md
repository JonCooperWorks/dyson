# Chat Persistence

Dyson persists per-chat conversation history so context survives restarts.
The Telegram controller maintains one agent per chat with full message
history. History is saved to disk after each turn and restored on startup.

**Key files:**
- `src/chat_history/mod.rs` — `ChatHistory` trait
- `src/chat_history/disk.rs` — `DiskChatHistory` (per-chat directory persistence)
- `src/chat_history/migrate.rs` — one-shot flat-layout migration
- `src/controller/telegram.rs` — per-chat agent management, `/clear` and `/memory` commands
- `src/agent/mod.rs` — `Agent::messages()`, `Agent::set_messages()`, `Agent::clear()`

---

## ChatHistory Trait

```rust
pub trait ChatHistory: Send + Sync {
    fn save(&self, chat_id: &str, messages: &[Message]) -> Result<()>;
    fn load(&self, chat_id: &str) -> Result<Vec<Message>>;
    fn save_title(&self, chat_id: &str, title: &str) -> Result<()>;
    fn load_title(&self, chat_id: &str) -> Result<Option<String>>;
    fn remove_title(&self, chat_id: &str) -> Result<()>;
    fn rotate(&self, chat_id: &str) -> Result<()>;
    fn remove(&self, chat_id: &str) -> Result<()>;
    fn list(&self) -> Result<Vec<String>>;
}
```

The only configured production backend today is:

| Backend | Use case |
|---------|----------|
| `DiskChatHistory` (default) | One directory per chat in `~/.dyson/chats/` |

`rotate()` archives the current conversation (preserves the file with a
timestamp suffix) and starts fresh — used by `/clear`.  Old history files
are preserved for review or future RAG indexing.

---

## DiskChatHistory

The default implementation stores one directory per chat:

```
~/.dyson/chats/
  2102424765/
    transcript.json
    title.txt
    feedback.json
    archives/
      2026-03-19T14-30-00.json
    media/
    artefacts/
    files/
```

`transcript.json` contains a JSON array of `Message` objects with the current
conversation history. Large inline image/document payloads are externalized to
`media/*.b64` and restored on load so the transcript stays small.

On startup, `DiskChatHistory::new` migrates the older flat layout
(`{id}.json`, `{id}.TIMESTAMP.json`, `{id}_feedback.json`, `{id}_media/`, and
shared `artefacts/`) into this per-chat shape.

---

## Telegram Commands

| Command | Effect |
|---------|--------|
| `/clear` | Archive conversation history, clear in-memory messages, start fresh |
| `/memory <note>` | Append a timestamped note to `MEMORY.md` in the workspace |
| `/whoami` | Reply with the chat ID (no LLM call) |

---

## How It Works

1. **First message from a chat**: Agent is created, disk history is loaded
   (if any), and `agent.set_messages()` restores the conversation.

2. **Each message**: The per-chat agent runs with full context from previous
   turns. After the agent responds, `chat_store.save()` writes the updated
   history to disk.

3. **`/clear`**: Clears the in-memory agent, calls `chat_store.rotate()`, and
   re-seeds an empty current transcript for controllers that need the chat to
   remain visible. The next message starts a fresh conversation while the old
   transcript remains under `archives/`.

4. **Delete conversation**: HTTP `DELETE /api/conversations/:id` hard-deletes
   empty chats with `remove()`. Non-empty chats are rotated instead, so a
   transcript worth preserving is archived rather than discarded.

5. **Config reload**: When `dyson.json` or workspace files change, all
   in-memory agents are cleared (they'll be recreated with the new config
   on the next message). On-disk history is preserved and restored.

6. **Service restart**: On startup, no agents exist in memory. When a chat
   sends a message, the agent is created and history is loaded from disk
   automatically.

---

See also: [Agent Loop](agent-loop.md) ·
[Architecture Overview](architecture-overview.md) ·
[Configuration](configuration.md)
