# AST Facade (`ast`)

## Overview
Compatibility facade that re-exports the `dyson-ast` crate (language detection, tree-sitter parsing, and AST services) under `crate::ast`.

## Interface
- `pub use dyson_ast::*;` [mod.rs]

## Invariants
- None (pure re-export; behaviour is defined in `dyson-ast`) [mod.rs]

## Side Effects
- None [mod.rs]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
