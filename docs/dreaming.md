# Dreaming — Autonomous Background Cognition

Background cognitive tasks that run concurrently with the main agent loop
but **never block it**.  Memory consolidation, self-improvement, learning
synthesis — housekeeping that happens alongside waking interactions.

## The Contract

> Dreams operate outside of the controller loop.  They must never block it.

Everything follows from this:

1. The main loop sends events over a channel to a **persistent dream
   thread** (`dyson-dreams`) and never blocks.
2. The dream thread summarises the conversation, then `tokio::spawn`s
   matching dreams as fire-and-forget tasks.
3. Dreams access the LLM through a **rate-limited handle** at `Background`
   priority — they can never starve the main loop.
4. Dreams communicate only through the **shared workspace**
   (`Arc<RwLock<Workspace>>`).  Nothing enters the conversation history.
5. Dreams use `SilentOutput` — stream events are consumed, never shown.

## Architecture

```
┌─────────────────────────────────────┐
│         Agent (waking loop)         │
│  run_inner() → LLM → tools → ...   │
│         │                           │
│    dream_handle.fire(event, ...)    │
│    (channel send, returns instantly)│
└─────────┼───────────────────────────┘
          │  mpsc channel
          ▼
┌─────────────────────────────────────┐
│      Dream Thread (persistent)      │
│  "dyson-dreams" std::thread         │
│  rx.recv() → summarise → spawn     │
└─────────┼───────────────────────────┘
          │  tokio::spawn (fire-and-forget)
          ▼
┌─────────────────────────────────────┐
│        Dream (background task)      │
│  rate-limited LLM, workspace I/O   │
└─────────────────────────────────────┘
```

## Rate Limiting

Dreams share the same sliding-window rate limiter as the main loop:

| Priority     | Capacity | Use case                              |
|--------------|----------|---------------------------------------|
| `UserFacing` | 100%     | Interactive agent loop                |
| `Background` | 66%      | Dreams                                |
| `Scheduled`  | 33%      | Future: heartbeat/cron, batch tasks   |

A dream that hits the limit stops early — no retry, no blocking.

```
RateLimited<Box<dyn LlmClient>>          (ClientRegistry owns)
    └── handle(UserFacing)                → RateLimitedHandle (agent gets)
            ├── access()                  → UserFacing (100%)
            └── with_priority(Background) → RateLimitedHandle (dreams get)
                    └── access()          → Background (66%)
```

## Core Types

All in `src/agent/dream.rs`.

### Triggers and Events

| `DreamTrigger`      | Fires when                                    |
|---------------------|-----------------------------------------------|
| `EveryNTurns(n)`    | User turn count is a multiple of `n`          |
| `AfterCompaction`   | Context compaction just ran                   |
| `OnSessionEnd`      | Session ending (clear / teardown)             |

| `DreamEvent`                      | Emitted from          |
|-----------------------------------|-----------------------|
| `TurnComplete { turn_count }`     | Start of `run_inner()`|
| `Compaction`                      | `compact()`           |
| `SessionEnd`                      | `clear()`             |

### `Dream` trait

```rust
#[async_trait]
pub trait Dream: Send + Sync {
    fn name(&self) -> &str;
    fn trigger(&self) -> DreamTrigger;
    async fn run(&self, ctx: DreamContext) -> Result<DreamOutcome>;
}
```

### `DreamContext`

Owned by each spawned dream — no borrows from the agent:

- `client: RateLimitedHandle<Box<dyn LlmClient>>`
- `config: CompletionConfig`
- `tool_context: ToolContext`
- `conversation_summary: String`
- `turn_count: usize`

### `DreamHandle`

Channel-based handle held by the agent.  `fire()` sends a `DreamRequest`
over `mpsc` and returns immediately.  The dream thread receives it,
builds the conversation summary, and spawns matching dreams.

### `DreamRunner` (internal)

Lives inside the dream thread.  Iterates registered dreams, checks
triggers, and `tokio::spawn`s matches.  Not exposed to the agent.

## Built-in Dreams

All in `src/agent/reflection.rs`.

| Dream                      | Trigger                        | What it does                                           |
|----------------------------|--------------------------------|--------------------------------------------------------|
| `LearningSynthesisDream`   | `AfterCompaction`              | Single LLM call to merge learnings into `MEMORY.md`   |
| `MemoryMaintenanceDream`   | `EveryNTurns(nudge_interval)`  | Mini agent loop (5 iters) updating memory files        |
| `SelfImprovementDream`     | `EveryNTurns(nudge_interval*2)`| Mini agent loop (3 iters) creating skills / exporting  |

## Implementing a Custom Dream

```rust
use async_trait::async_trait;
use crate::agent::dream::*;
use crate::error::Result;

pub struct MyDream;

#[async_trait]
impl Dream for MyDream {
    fn name(&self) -> &str { "my-dream" }
    fn trigger(&self) -> DreamTrigger { DreamTrigger::EveryNTurns(10) }

    async fn run(&self, ctx: DreamContext) -> Result<DreamOutcome> {
        let start = std::time::Instant::now();
        let client = ctx.client.access()?;
        // client.stream(...).await?
        Ok(DreamOutcome {
            dream_name: self.name().to_string(),
            actions_taken: 1,
            duration: start.elapsed(),
            artifacts: vec!["did something useful".to_string()],
        })
    }
}
```

Register in `Agent::new()`:

```rust
dreams.push(Arc::new(MyDream));
```

## Non-blocking Guarantee

Enforced at every level:

1. **`DreamHandle.fire()`** — channel send, returns instantly.  Only cost
   on the main thread is cloning the message vector.
2. **Persistent dream thread** — summarisation and spawning happen on
   `dyson-dreams`, never on the main loop.
3. **Owned context** — no borrows from the agent survive into spawned tasks.
4. **Rate limiting** — dreams at `Background` priority (66%) can't starve
   the main loop.  If the window is full they stop early.
5. **`Arc<RwLock<Workspace>>`** — workspace writes are fast; the agent only
   takes the read lock for system prompt injection.
6. **`SilentOutput`** — dreams never write to the user's output stream.

If a dream panics, its `tokio::spawn` task unwinds independently.  If the
dream thread itself panics while processing a request, `catch_unwind`
catches it, logs the panic, and continues processing the next request —
no single bad dream can kill the thread.

## File Map

| File                         | Contents                                          |
|------------------------------|---------------------------------------------------|
| `src/agent/dream.rs`        | `Dream` trait, `DreamHandle`, `DreamRunner`, types |
| `src/agent/reflection.rs`   | Built-in dream implementations                    |
| `src/agent/rate_limiter.rs` | `Priority`, `RateLimited`, `RateLimitedHandle`     |
| `src/agent/silent_output.rs`| `SilentOutput` — no-op output for dreams           |
| `src/agent/mod.rs`          | `fire_dreams()`, `DreamHandle` field               |
