# Dreaming — Autonomous Background Cognition

Dreaming is the agent's subconscious: background cognitive tasks that run
concurrently with the main agent loop but **never block it**.  Memory
consolidation, self-improvement, learning synthesis — all the housekeeping
that happens alongside (or between) waking interactions.

## The Contract

> Dreams operate outside of the controller loop.  They should never block it.

This is the single inviolable rule.  Everything else follows from it:

1. Dreams are **spawned** via `tokio::spawn` — fire-and-forget.
2. Dreams access the LLM through a **rate-limited handle** at `Background`
   priority — they share the provider's rate limit with the main loop but
   can never starve it.
3. Dreams use `SilentOutput` — their stream events are consumed but never
   shown to the user.
4. Dreams communicate only through the **shared workspace**
   (`Arc<RwLock<Workspace>>`).  Nothing enters the main conversation history.
5. The `DreamRunner.fire()` method returns immediately after spawning.

## Architecture

```
┌─────────────────────────────────────┐
│         Agent (waking loop)         │
│  client.access() at UserFacing      │
│  run_inner() → LLM → tools → ...   │
│         │                           │
│    DreamRunner.fire(event)          │
│         │                           │
└─────────┼───────────────────────────┘
          │  tokio::spawn (fire-and-forget)
          ▼
┌─────────────────────────────────────┐
│         Dream (background)          │
│  client.access() at Background      │
│  same rate counter, lower priority  │
│  reads/writes workspace via Arc     │
│  never blocks the agent loop        │
└─────────────────────────────────────┘
```

## Rate Limiting and Priority

Dreams share the same rate limiter as the main agent loop.  The rate
limiter uses a sliding-window algorithm with three priority levels:

| Priority      | Effective capacity | Use case                               |
|---------------|--------------------|----------------------------------------|
| `UserFacing`  | 100% of max_calls  | Interactive agent loop (user waiting)  |
| `Background`  | 66% of max_calls   | Dreams (memory, learning, improvement) |
| `Scheduled`   | 33% of max_calls   | Future: heartbeat/cron, batch tasks    |

Lower-priority callers voluntarily cap themselves so higher-priority
callers always have headroom.  A dream that hits the rate limit simply
stops early — it doesn't retry or block.

**There is no way to reach the LLM without going through the rate limiter.**
Dreams receive a `RateLimitedHandle<Box<dyn LlmClient>>` — a cloneable,
priority-locked handle to the same `Arc<LlmClient>` that the agent uses.
The handle's `access()` method checks the shared rate counter before
returning a guard that dereferences to the client.

```
RateLimited<Box<dyn LlmClient>>     (agent owns this)
    │
    ├── access()                     → UserFacing priority (100%)
    │
    └��─ handle(Background)           → RateLimitedHandle (dreams get this)
            │
            └── access()             → Background priority (66%)
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

- `client: RateLimitedHandle<Box<dyn LlmClient>>` — rate-limited LLM access
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

        // Access the LLM through the rate-limited handle.
        // This checks the shared rate counter at Background priority.
        let client = ctx.client.access()?;

        // Make your LLM call...
        // client.stream(&messages, system, "", &tools, &ctx.config).await?

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
3. **Shared LLM client via `Arc`** — dreams access the same client through
   a `RateLimitedHandle`, which holds `Arc<Box<dyn LlmClient>>`.  No
   separate connections, no rate-limiter bypass.
4. **Priority-aware rate limiting** — dreams operate at `Background`
   priority (66% capacity).  If the window is full, they stop early
   rather than competing with the user-facing loop.
5. **`Arc<RwLock<Workspace>>`** — the only other shared state.  Workspace
   reads/writes are fast (file I/O, SQLite FTS5) and non-blocking in
   practice.  Even if a dream holds the write lock briefly, the agent
   loop only takes the read lock for workspace system prompt injection.
6. **`SilentOutput`** — dreams never write to the user's output stream.

If a dream panics, the `tokio::spawn` task unwinds independently — the
agent loop is unaffected.

## File Map

| File                          | Contents                                    |
|-------------------------------|---------------------------------------------|
| `src/agent/dream.rs`         | `Dream` trait, `DreamRunner`, trigger types  |
| `src/agent/reflection.rs`    | Built-in dream implementations               |
| `src/agent/rate_limiter.rs`  | `Priority`, `RateLimited`, `RateLimitedHandle` |
| `src/agent/silent_output.rs` | `SilentOutput` — no-op output for dreams     |
| `src/agent/mod.rs`           | `fire_dreams()` helper, `DreamRunner` field  |
| `docs/dreaming.md`           | This document                                |
