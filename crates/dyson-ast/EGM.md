# dyson-ast

## Overview
The `dyson-ast` crate provides language parsing, structural navigation and taint primitives for Dyson ([Cargo.toml:description]). It statically links tree-sitter grammars for Rust, Python, JavaScript, TypeScript, Go, Java, C, C++, C#, Ruby, Kotlin, Swift, Zig, Elixir, Erlang, OCaml, Haskell, Nix and JSON. It depends on `dyson-core`, `ignore`, `tokio` and `thiserror` ([Cargo.toml:dependencies]).

## Subdirectories & Boundaries
- src: [src/EGM.md](./src/EGM.md). Contains the grammar registry (`LanguageConfig`, `try_parse_file`, `walk_dir`), the definition and identifier search helpers, and the `taint` submodule (`build_index`, `trace`) for cross-file source-to-sink reachability.

## Verification
- Run: `cargo test -p dyson-ast` (from `dyson/`)
