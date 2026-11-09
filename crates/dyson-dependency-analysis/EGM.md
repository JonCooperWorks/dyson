# dyson-dependency-analysis

## Overview
The `dyson-dependency-analysis` crate handles dependency manifest discovery, parsing and OSV analysis for Dyson ([Cargo.toml:description]). It depends on `dyson-core`, `reqwest`, `futures-util`, `tokio`, `ignore`, `regex`, `serde_json`, `serde_yaml_ng` and `toml` ([Cargo.toml:dependencies]).

## Subdirectories & Boundaries
- src: [src/EGM.md](./src/EGM.md). Contains `scan`, which walks a root, chooses a parser per manifest filename, sends dependencies with exact versions or PURLs to `https://api.osv.dev`, and returns a `ScanReport`. It also contains the `OsvClient` and the per-ecosystem `ManifestParser` implementations.

## Verification
- Run: `cargo test -p dyson-dependency-analysis` (from `dyson/`)
