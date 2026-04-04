# Dreaming — Autonomous Background Cognition

Dreaming is the agent's subconscious: background cognitive tasks that run
concurrently with the main agent loop but **never block it**.  Memory
consolidation, self-improvement, learning synthesis — all the housekeeping
that happens alongside (or between) waking interactions.

## The Contract

> Dreams operate outside of the controller loop.  They should never block it.

This is the single inviolable rule.  Everything else follows from it:

1. Dreams are **spawned** via `tokio::spawn` — fire-and-forget.
2. Dreams build their **own LLM client** so they don't contend with the
   main agent's rate limiter.
3. Dreams use `SilentOutput` — their stream events are consumed but never
   shown to the user.
4. Dreams communicate only through the **shared workspace**
   (`Arc<RwLock<Workspace>>`).  Nothing enters the main conversation history.
5. The `DreamRunner.fire()` method returns immediately after spawning.

## Architecture

```
┌─────────────────────────────────────┐
│         Agent (waking loop)         │
│  run_inner() → LLM → tools → ...   │
│         │                           │
│    DreamRunner.fire(event)          │
│         │                           │
└─────────┼───────────────────────────┘
          │  tokio::spawn (fire-and-forget)
          ▼
┌─────────────────────────────────────┐
│         Dream (background)          │
│  own LLM client, SilentOutput       │
│  reads/writes workspace via Arc     │
│  never blocks the agent loop        │
└─────────────────────────────────────┘
```

## Core Types

All types live in `src/agent/dream.rs`.

### `DreamTrigger`

When a dream should activate:

| Variant             | Fires when                                       |
|---------------------|--------------------------------------------------|
| `EveryNTurns(n)`    | The user turn count is a multiple of `n`         |
| `AfterCompaction`   | Context compaction just condensed the history    |
| `OnSessionEnd`      | The session is ending (clear / teardown)         |

### `DreamEvent`

Events emitted by the agent loop:

| Variant                           | Emitted from                  |
|-----------------------------------|-------------------------------|
| `TurnComplete { turn_count }`     | Start of `run_inner()`        |
| `Compaction`                      | Inside `compact()`            |
| `SessionEnd`                      | Inside `clear()`              |

### `DreamContext`

Everything a dream needs to run autonomously — built by the agent and
moved into the spawned task:

- `settings: AgentSettings` — to build a fresh LLM client
- `config: CompletionConfig` — model, max_tokens, temperature
- `tool_context: ToolContext` — workspace, working dir, cancellation
- `conversation_summary: String` — condensed conversation (not full history)
- `turn_count: usize` — how many user turns have been processed

### `DreamOutcome`

What a dream did — logged for observability:

- `dream_name: String`
- `actions_taken: usize`
- `duration: Duration`
- `artifacts: Vec<String>` — human-readable descriptions of changes

### `Dream` trait

```rust
#[async_trait]
pub trait Dream: Send + Sync {
    fn name(&self) -> &str;
    fn trigger(&self) -> DreamTrigger;
    async fn run(&self, ctx: DreamContext) -> Result<DreamOutcome>;
}
```

### `DreamRunner`

Holds `Vec<Arc<dyn Dream>>` and exposes a single method:

```rust
pub fn fire(&self, event: &DreamEvent, ctx_factory: impl Fn() -> DreamContext)
```

For each registered dream whose trigger matches the event, it calls
`ctx_factory()` to build a context and `tokio::spawn`s `dream.run(ctx)`.
Returns immediately — the caller is never blocked.

## Built-in Dreams

All three live in `src/agent/reflection.rs`.

### 1. `LearningSynthesisDream`

- **Trigger**: `AfterCompaction`
- **What**: Single LLM call to merge conversation learnings into `MEMORY.md`.
  No tools — just a prompt asking the LLM to synthesise new information
  into the existing memory file.
- **Why**: Before compaction condenses the conversation, capture insights
  that might be lost.

### 2. `MemoryMaintenanceDream`

- **Trigger**: `EveryNTurns(nudge_interval)` (default: 5)
- **What**: Mini agent loop (up to 5 iterations) with four workspace tools
  (`workspace_view`, `workspace_update`, `workspace_search`, `memory_search`).
  Reviews the conversation and makes targeted updates to `MEMORY.md`,
  `USER.md`, and overflow notes.
- **Why**: Periodic consolidation keeps memory fresh and prevents information
  loss across long sessions.

### 3. `SelfImprovementDream`

- **Trigger**: `EveryNTurns(nudge_interval * 2)` with a minimum-turn gate
- **What**: Mini agent loop (up to 3 iterations) with `skill_create` and
  `export_conversation` tools.  Decides whether to create/improve skills
  or export training data.
- **Why**: The agent gets better over time by encoding reusable procedures
  as skills and capturing high-quality interactions as training data.

## Implementing a Custom Dream

```rust
use async_trait::async_trait;
use crate::agent::dream::*;
use crate::error::Result;

pub struct MyDream;

#[async_trait]
impl Dream for MyDream {
    fn name(&self) -> &str {
        "my-dream"
    }

    fn trigger(&self) -> DreamTrigger {
        DreamTrigger::EveryNTurns(10)
    }

    async fn run(&self, ctx: DreamContext) -> Result<DreamOutcome> {
        let start = std::time::Instant::now();

        // Build your own LLM client — never share the agent's.
        let client = crate::llm::create_client(&ctx.settings, None, false);

        // Do your work...

        Ok(DreamOutcome {
            dream_name: self.name().to_string(),
            actions_taken: 1,
            duration: start.elapsed(),
            artifacts: vec!["did something useful".to_string()],
        })
    }
}
```

Register it in `Agent::new()`:

```rust
dream_runner.add(Arc::new(MyDream));
```

## Non-blocking Guarantee

The non-blocking guarantee is enforced at multiple levels:

1. **`DreamRunner.fire()`** — spawns tasks and returns immediately.
2. **Owned context** — `DreamContext` is fully cloned/owned.  No borrows
   from the agent survive into the spawned task.
3. **Separate LLM client** — each dream builds its own via
   `create_client()`, so there's no rate-limiter contention.
4. **`Arc<RwLock<Workspace>>`** — the only shared state.  Workspace
   reads/writes are fast (file I/O, SQLite FTS5) and non-blocking in
   practice.  Even if a dream holds the write lock briefly, the agent
   loop only takes the read lock for workspace system prompt injection.
5. **`SilentOutput`** — dreams never write to the user's output stream.

If a dream panics, the `tokio::spawn` task unwinds independently — the
agent loop is unaffected.

## File Map

| File                          | Contents                                    |
|-------------------------------|---------------------------------------------|
| `src/agent/dream.rs`         | `Dream` trait, `DreamRunner`, trigger types  |
| `src/agent/reflection.rs`    | Built-in dream implementations               |
| `src/agent/silent_output.rs` | `SilentOutput` — no-op output for dreams     |
| `src/agent/mod.rs`           | `fire_dreams()` helper, `DreamRunner` field  |
| `docs/dreaming.md`           | This document                                |
