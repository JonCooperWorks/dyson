# Dyson Blueprint

## System Overview

Dyson is a streaming AI agent runtime in Rust: it streams model output, detects
tool calls, gates them through a sandbox, feeds results back to the model, and
repeats until the task is done. It runs standalone (terminal, HTTP/web,
Telegram controllers) or as a Swarm-managed agent whose config, tokens and
durable state are supplied by `dyson-swarm`. The human overview lives in
[README.md](./README.md).

The Cargo workspace has six members [Cargo.toml:workspace.members]; shared
contracts come from `dyson-common` pinned to a release tag [Cargo.toml:workspace.dependencies].

## Subdirectories & System Boundaries

- crates: [./crates/EGM.md](./crates/EGM.md) - the six workspace crates (`dyson` composition root plus core, harness, persistence, ast, dependency-analysis)
- deploy: [./deploy/EGM.md](./deploy/EGM.md) - managed-Dyson container entrypoint that sets up tmpfs-only Codex/Claude credential dirs, then execs `dyson`
- scripts: [./scripts/EGM.md](./scripts/EGM.md) - operator update scripts (`update.sh`, `update-swarm.sh`)

## Global Invariants

- Every tool call passes through the sandbox; it can be disabled only with `--dangerous-no-sandbox`, and startup aborts when the OS backend is missing [crates/dyson/src/sandbox/mod.rs:create_sandbox]
- Workspace crates must share one `reqwest` major with `dyson-common` and `dyson-swarm`, because the shared OAuth transport exposes `&reqwest::Client` [crates/dyson/Cargo.toml:dependencies]
- The web UI is a required build input; `cargo build` runs the frontend tests and fails if they fail [crates/dyson/build.rs:run_npm_build]

## Global Side Effects

- Outbound HTTPS to LLM providers and MCP servers; `claude`/`codex` subprocesses for CLI-backed providers [crates/dyson/src/llm/EGM.md]
- Local filesystem state: `dyson.json`, the workspace (Markdown plus SQLite FTS5 `memory.db`), chat history and run journals, `~/.dyson` OAuth tokens [crates/dyson/src/workspace/EGM.md] [crates/dyson-persistence/EGM.md]
- Build-time `npm ci` / `npm run build` [crates/dyson/build.rs:main]

## System Verification

- Run: `cd dyson && cargo test --workspace`
