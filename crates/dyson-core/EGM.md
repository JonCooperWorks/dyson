# dyson-core

## Overview
The `dyson-core` crate holds the provider-neutral domain contracts and error model for Dyson ([Cargo.toml:description]). Its only dependencies are `reqwest`, `serde`, `serde_json` and `thiserror` ([Cargo.toml:dependencies]). The other workspace crates in this partition (`dyson-ast`, `dyson-dependency-analysis`, `dyson-persistence`) depend on it.

## Subdirectories & Boundaries
- src: [src/EGM.md](./src/EGM.md). Contains `DysonError`/`Result` with `sanitized_message`, and the `Message`/`ContentBlock`/`Role`/`Artefact` conversation model, including its serde tags and token estimators.

## Verification
- Run: `cargo test -p dyson-core` (from `dyson/`)
