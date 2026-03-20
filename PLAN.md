# Hermes-Inspired Tiered Memory System — Implementation Plan

## Overview

Add a three-tier memory architecture to Dyson:

- **Tier 1 (always in context):** MEMORY.md and USER.md with enforced character limits, included in every system prompt
- **Tier 2 (searchable via FTS5):** SQLite full-text search over overflow content, queried via a new `memory_search` tool
- **Tier 3 (journals):** Existing daily journals (unchanged)

Plus periodic **memory nudges** injected as user messages every N turns.

## Step 1: Add `MemoryConfig` to config system

**Files:** `src/config/mod.rs`, `src/config/loader.rs`

Add `MemoryConfig` struct to `WorkspaceConfig`:

```rust
pub struct MemoryConfig {
    pub limits: HashMap<String, usize>,  // file -> max chars
    pub nudge_interval: usize,           // every N turns, 0 = disabled
}
// Default: MEMORY.md=2200, USER.md=1375, nudge_interval=5
```

Add to `WorkspaceConfig`:
```rust
pub memory: MemoryConfig,
```

In `loader.rs`, add optional `memory` field to `JsonWorkspace` with `limits` (HashMap) and `nudge_interval` (usize). Merge with defaults for unspecified fields.

## Step 2: Extend Workspace trait with `char_limit()` and `nudge_interval()`

**File:** `src/workspace/mod.rs`

Add two default methods to the `Workspace` trait:

```rust
fn char_limit(&self, _file: &str) -> Option<usize> { None }
fn nudge_interval(&self) -> usize { 5 }
```

## Step 3: Implement in both workspace backends

**File:** `src/workspace/openclaw.rs`

- Add `memory_config: MemoryConfig` field to `OpenClawWorkspace`
- Thread `MemoryConfig` through `load()`, `load_default()`, `load_from_connection_string()`
- Implement `char_limit()` and `nudge_interval()` using the config

**File:** `src/workspace/in_memory.rs`

- Add `limits: HashMap<String, usize>` and `nudge_interval: usize` fields
- Add builder method `.with_limit(file, max_chars)` for testing
- Implement trait methods

**File:** `src/workspace/mod.rs` (factory)

- Update `create_workspace()` to pass `config.memory.clone()` through

## Step 4: Add USER.md to workspace defaults and system prompt

**File:** `src/workspace/openclaw.rs`

- Add USER.md creation in `ensure_defaults()` with a starter template
- Add `("USER PROFILE", "USER.md")` to the system prompt composition loop (after MEMORY.md)

**File:** `src/workspace/in_memory.rs`

- Add `("USER PROFILE", "USER.md")` to `system_prompt()`

## Step 5: Enforce character limits in `workspace_update`

**File:** `src/tool/workspace_update.rs`

Before writing, check the limit:
- Compute the would-be content (set mode: new content; append mode: existing + new)
- If `chars().count() > limit`, return an error with current usage, limit, and guidance to consolidate or overflow
- On success, include usage stats in the response: `[current/max chars used]`

## Step 6: Add SQLite FTS5 for Tier 2 memory search

**File:** `Cargo.toml`

- Add `rusqlite = { version = "0.32", features = ["bundled"] }`

**File:** `src/workspace/memory_store.rs` (new)

Create a `MemoryStore` struct wrapping a SQLite database:

```rust
pub struct MemoryStore {
    conn: rusqlite::Connection,
}

impl MemoryStore {
    pub fn open(path: &Path) -> Result<Self>      // opens/creates DB at path
    pub fn index(&self, key: &str, content: &str)  // upsert into FTS5 table
    pub fn search(&self, query: &str) -> Vec<SearchResult>  // FTS5 MATCH query
    pub fn remove(&self, key: &str)                // delete from index
}
```

Schema: `CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(key, content)`

**File:** `src/workspace/openclaw.rs`

- Add `memory_store: MemoryStore` field
- Initialize from `{workspace_path}/memory.db`
- When `set()`/`append()` is called on `memory/**` files, also index into the store
- On `load()`, index all existing `memory/` files

**File:** `src/tool/memory_search.rs` (new)

New tool `memory_search`:
- Input: `{ "query": "string" }`
- Runs FTS5 search against the memory store
- Returns matching file keys + snippet highlights
- Falls back to existing regex search if FTS5 returns nothing

Register the tool in the builtin skill.

## Step 7: Add memory nudges to the agent loop

**File:** `src/agent/mod.rs`

- Add `turn_count: usize` and `nudge_interval: usize` fields to `Agent`
- Accept `nudge_interval` parameter in `Agent::new()`
- In `run()`, increment `turn_count` after pushing the user message
- When `nudge_interval > 0 && turn_count % nudge_interval == 0`, inject a nudge as a user message before the LLM call

Nudge content:
```
[System: Memory Maintenance] Consider saving important details from this conversation.
MEMORY.md: {current}/{max} chars. USER.md: {current}/{max} chars.
Use workspace_view/workspace_update. Move overflow to memory/notes/ (searchable via memory_search).
```

**File:** `src/controller/mod.rs`

- In `build_agent()`, read nudge interval from workspace and pass to `Agent::new()`

## Step 8: Update tool descriptions

**File:** `src/tool/workspace_update.rs`

Update description to mention character limits on MEMORY.md and USER.md.

## Step 9: Tests

1. **Limit enforcement**: Write exceeding limit → error with usage stats
2. **Under-limit writes**: Write under limit → success with usage info
3. **Append limit checking**: Combined size checked, not just new content
4. **USER.md in system prompt**: Verify `system_prompt()` includes USER.md
5. **`char_limit()` trait**: Returns correct limits for configured files, `None` for others
6. **Nudge generation**: Unit test nudge message construction
7. **InMemoryWorkspace with limits**: Test `.with_limit()` builder
8. **MemoryStore FTS5**: Index, search, remove operations
9. **memory_search tool**: End-to-end search through tool interface

## Sequencing

1. Step 1 (config) — foundation
2. Steps 2-3 (trait + impls) — depends on Step 1
3. Step 4 (USER.md) — can parallel with 2-3
4. Step 5 (limit enforcement) — depends on 2-3
5. Step 6 (SQLite FTS5) — depends on 2-3
6. Step 7 (nudges) — depends on 2-3
7. Steps 8-9 (descriptions, tests) — after all above
