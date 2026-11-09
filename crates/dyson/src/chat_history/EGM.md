# Chat History (`chat_history`)

## Overview
Application facade over `dyson-persistence` plus a factory that builds the configured per-chat persistence backend.

## Interface
- Re-exports `ChatHistory`, `DiskChatHistory` from `dyson_persistence` [mod.rs]
- `create_chat_history(config: &ChatHistoryConfig) -> Result<Box<dyn ChatHistory>>` [mod.rs:create_chat_history]
- `coalesce::CoalescingPersister` (crate re-export) [mod.rs:coalesce]

## Invariants
- Only backend `"disk"` is supported; any other value is a config error [mod.rs:create_chat_history]

## Side Effects
- The disk backend reads/writes chat files under `connection_string` (implemented in `dyson-persistence`) [mod.rs:create_chat_history]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
