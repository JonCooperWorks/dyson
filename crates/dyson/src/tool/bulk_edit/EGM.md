# Bulk Edit Tool (`tool/bulk_edit`)

## Overview
The `bulk_edit` tool performs multi-file edits dispatched by an `operation` field: AST-aware `rename_symbol` (with word-boundary text fallback for non-grammar files), plain-text or regex `find_replace`, and AST-only `list_definitions`. AST support comes from `crate::ast` (a facade over `dyson-ast`).

## Interface
- `BulkEditTool` - `Tool` named `bulk_edit`; `operation` enum `rename_symbol` | `find_replace` | `list_definitions` [mod.rs:BulkEditTool]
- `rename_symbol(resolved, working_dir, old_name, new_name, dry_run)` [rename.rs:rename_symbol]
- `find_replace(...)` and `validate(...)` [find_replace.rs:find_replace, find_replace.rs:validate]
- `list_definitions(resolved_path, working_dir) -> Result<ToolOutput>` [definitions.rs:list_definitions]

## Invariants
- Target paths are resolved through `ctx.resolve_path` before any operation [mod.rs:BulkEditTool]
- `find_replace` edits at most `MAX_FILES = 200` files [find_replace.rs:MAX_FILES]; rename and definitions use `ast::MAX_FILES` [rename.rs:rename_symbol, definitions.rs:list_definitions]
- Files larger than `ast::MAX_FILE_SIZE` are skipped [find_replace.rs:find_replace, rename.rs:rename_symbol]
- `dry_run` performs no writes [find_replace.rs:find_replace, rename.rs:rename_symbol]

## Side Effects
- Rewrites matched files in place with `std::fs::write` when not a dry run [find_replace.rs:find_replace, rename.rs:rename_symbol]
- `list_definitions` runs on `spawn_blocking` [mod.rs:BulkEditTool]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
