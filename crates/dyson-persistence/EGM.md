# dyson-persistence

## Overview
The `dyson-persistence` crate provides durable conversation and execution-journal persistence for Dyson ([Cargo.toml:description]). It depends on `dyson-common`, `dyson-core`, `dyson-harness`, `serde_json`, `tokio` and `tracing` ([Cargo.toml:dependencies]).

## Subdirectories & Boundaries
- src: [src/EGM.md](./src/EGM.md). Contains the `ChatHistory` trait, `DiskChatHistory` (per-chat directories, atomic transcript writes, fsynced JSONL run journal), `CoalescingPersister` (debounced background checkpoints), and the chats-directory `migrate`.

## Verification
- Run: `cargo test -p dyson-persistence` (from `dyson/`)
