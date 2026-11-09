# dyson-harness

## Overview
The `dyson-harness` crate holds the deterministic execution, scheduling, replay and evaluation contracts for Dyson agents ([Cargo.toml:description]). Its only dependencies are `rand`, `serde` and `serde_json` ([Cargo.toml:dependencies]). `dyson-persistence` depends on it for the run-event journal types.

## Subdirectories & Boundaries
- src: [src/EGM.md](./src/EGM.md). Contains `ToolCall`, `ToolExecutionPlan`, the `DependencyAnalyzer` phase scheduler, the pure `RunCursor` state machine, and the versioned `RunEvent` protocol with `evaluate_run` and `unresolved_tool_outcomes`.

## Verification
- Run: `cargo test -p dyson-harness` (from `dyson/`)
