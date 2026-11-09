# dyson (crate)

## Overview

The `dyson` package is the composition root of the Dyson agent runtime: providers,
tools, controllers, configuration and the CLI. It depends on the other workspace
crates (`dyson-core`, `dyson-ast`, `dyson-dependency-analysis`, `dyson-harness`,
`dyson-persistence`) and on `dyson-common` / `dyson-telegram` [Cargo.toml:dependencies].
Its build script compiles the web UI and embeds the bundle into the binary.

## Interface

- [src](./src/EGM.md) - library and binary source: agent loop, LLM clients, sandbox, skills, tools, controllers, config, workspace
- [examples](./examples/EGM.md) - AST/taint smoke tests, live security-review runs, benchmark scorer
- Feature `cedar` (default) - Cedar policy layer over the tool sandbox [Cargo.toml:features]
- Build script inputs: `src/controller/http/web/{index.html,package.json,package-lock.json,vite.config.js,src/**}` [build.rs:watch_inputs]
- Generated `$OUT_DIR/web_assets.rs`: `ASSETS` slice and `lookup_asset(key)` [build.rs:generate_asset_table]

## Invariants

- The frontend is required: a missing `web/index.html` panics the build, and there is no feature flag to skip it [build.rs:main]
- `npm run build` (lint, vitest, vite build) runs only when a watched input is newer than `dist/` [build.rs:needs_rebuild]; a failing frontend test fails the cargo build [build.rs:run_npm_build]
- `npm ci` runs only when the `package-lock.json` hash differs from `node_modules/.dyson-lock-hash` [build.rs:ensure_node_modules]
- Source maps (`*.map`) are never embedded [build.rs:generate_asset_table]
- `tests/` holds integration tests only and has no contract

## Side Effects

- Build time: spawns `npm ci` and `npm run build` in `src/controller/http/web`, writes `node_modules/.dyson-lock-hash`, and writes `web_assets.rs` into `OUT_DIR` [build.rs:ensure_node_modules] [build.rs:run_npm_build] [build.rs:generate_asset_table]
- Runtime side effects are documented in [src](./src/EGM.md)

## Verification

- Run: `cd dyson && cargo test -p dyson`
