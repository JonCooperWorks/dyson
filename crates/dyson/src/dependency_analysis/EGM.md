# Dependency Analysis Facade (`dependency_analysis`)

## Overview
Compatibility facade that re-exports the `dyson-dependency-analysis` crate under `crate::dependency_analysis`.

## Interface
- `pub use dyson_dependency_analysis::*;` [mod.rs]

## Invariants
- None (pure re-export) [mod.rs]

## Side Effects
- None [mod.rs]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
