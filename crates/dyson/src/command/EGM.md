# CLI Subcommands (`command`)

## Overview
Implementations of the `dyson` binary's subcommands. `main.rs` defines the clap CLI and dispatches here; this module is declared only in the binary (`mod command;` in `main.rs`) and uses the library through `dyson::` paths.

## Interface
- `init::run(...)` - `dyson init [--noinput] [--daemonize] [--import-filesystem PATH] [--path PATH] [--env KEY=VALUE]... [--dangerous-no-sandbox]` [init.rs:run, main.rs:Commands]
- `listen::run(...)` - `dyson listen [-c CONFIG] [--dangerous-no-sandbox] [--provider P] [--base-url U] [--workspace W]`; starts all configured controllers, concurrently when more than one [listen.rs:run]
- `run::run(...)` - `dyson run <prompt>` single-shot invocation [run.rs:run]
- `hash_bearer::run(plaintext)` - `dyson hash-bearer <plaintext> | --stdin`; prints an Argon2id PHC string for `auth.hash` [hash_bearer.rs:run]
- `cost_backfill::run(...)` - `dyson cost-backfill [-c CONFIG] --swarm-url URL [--bearer B] [--dry-run]` via `message_cost_backfill::backfill_history` [cost_backfill.rs:run]
- `swarm::run()` - `dyson swarm` boot mode inside a Swarm-managed pod [swarm.rs:run]
- `dirs_config_path()`, `resolve_config_path(explicit)`, `apply_overrides(...)` [mod.rs:resolve_config_path, mod.rs:apply_overrides]

## Invariants
- Config resolution order: explicit flag > `~/.dyson/dyson.json` > `./dyson.json` > none [mod.rs:resolve_config_path]
- `--provider` selects a named provider, falling back to treating it as a provider type string [mod.rs:apply_overrides]
- `dyson swarm` reads the `SWARM_*` env envelope (`SWARM_BEARER_TOKEN`, `SWARM_PROXY_URL`, `SWARM_PROXY_TOKEN`, `SWARM_MODEL`, `SWARM_TASK`, `SWARM_NAME`, `SWARM_INSTANCE_ID`, `SWARM_STATE_SYNC_*`, ...), configures an OpenAI-compatible client at `<SWARM_PROXY_URL>/openrouter`, and runs without the native sandbox because the Kata VM is the sandbox [swarm.rs:run]
- `dyson swarm` defaults: bind `0.0.0.0:80`, home `/var/lib/dyson` (overridable by `DYSON_BIND` / `DYSON_HOME`) [swarm.rs:DEFAULT_BIND, swarm.rs:DEFAULT_DYSON_HOME]
- Plaintext bearer tokens never go in `dyson.json`; only the hash does [hash_bearer.rs:run]

## Side Effects
- `init` writes `~/.dyson/dyson.json` (`0o600`), copies the binary into `~/.dyson/bin/` (`0o755`) and symlinks it into `~/.local/bin/`, and can write/enable a systemd unit (`~/.config/systemd/user/dyson.service`, possibly via `sudo`) [init.rs:run]
- `swarm` synthesises a config and workspace skeleton (including `TASK.md`) then hands off to `listen` [swarm.rs:run]
- `listen`/`run` start controllers and make LLM calls [listen.rs:run, run.rs:run]
- `cost_backfill` rewrites stored message costs unless `--dry-run` [cost_backfill.rs:run]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
