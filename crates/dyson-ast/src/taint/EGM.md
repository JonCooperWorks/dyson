# dyson-ast taint

## Overview
This is a cross-file taint-reachability oracle for the `security_engineer` subagent. `build_index` walks a working directory once per language and records every function definition, call site and assignment in a `SymbolIndex`. Then `trace` runs a BFS from a source `file:line` to a sink `file:line` and returns ranked candidate call chains. Call resolution goes by name and arguments bind by position, so results are approximate by design. The module comments say the agent must check each hop with `read_file` before it files a finding ([mod.rs:module doc]).

## Interface
- `build_index(language, working_dir) -> Result<SymbolIndex>`: async. Runs `build_index_sync` on the blocking pool ([index.rs:build_index]).
- `is_stale(index, working_dir) -> bool`: true when any indexed file is missing, its mtime is unreadable, or its mtime is newer than the build ([index.rs:is_stale]).
- `MAX_FIELD_DEPTH` (5): the most segments kept when a field chain such as `a.b.c` is flattened into a tainted path. Extra segments are dropped from the leaf end ([index.rs:MAX_FIELD_DEPTH]).
- `TAINT_MAX_FILES` (5000): the per-language index ceiling. It is `pub` in `index` but not re-exported from `mod.rs` ([index.rs:TAINT_MAX_FILES]).
- `trace(index, config, working_dir, source_file, source_line, sink_file, sink_line, opts) -> Result<TraceResult, TraceError>` ([trace.rs:trace]).
- `TraceOptions { max_depth, max_paths, max_frontier }`: defaults are 8, 5 and 10_000 ([trace.rs:TraceOptions]).
- `TraceResult { paths: Vec<TaintPath>, truncated_frontier }` ([trace.rs:TraceResult]).
- `Confidence::{High, Medium, Low}`: `from_unresolved_ratio` returns the unresolved percentage and a tier. `as_str` returns `"HIGH"`, `"MEDIUM"` or `"LOW"` ([trace.rs:Confidence]).
- `TraceError` variants: `NoEnclosingFunction`, `SourceNotIndexed`, `SinkNotIndexed`, `ParseFailed`, `UnsupportedLanguage` ([trace.rs:TraceError]).
- Index data types: `FnId` (a `usize` index into `fn_defs`), `FnDef`, `CallSite`, `Assignment` and `SymbolIndex`. The `SymbolIndex` lookup tables are `by_name`, `calls_by_fn`, `assigns_by_fn`, `fn_by_file` and `file_mtimes`, plus the `truncated` and `unresolved_callees` fields ([types.rs:SymbolIndex]).
- `SymbolIndex::fn_enclosing(file, byte, line)`: returns the innermost function whose `def_range` contains `byte`. If none does, it falls back to the smallest function whose line range contains `line` ([types.rs:SymbolIndex::fn_enclosing]).
- Output types: `TaintPath { hops }` has the helpers `unresolved_hops`, `imprecise_bindings`, `depth` and `resolved_hops`. `Hop` carries a `HopKind::{Source, Resolved, ImpreciseBinding, Ambiguous, UnresolvedCallee, Sink}` ([types.rs:TaintPath], [types.rs:HopKind]).
- Re-exports: `mod.rs` re-exports `MAX_FIELD_DEPTH`, `build_index`, `is_stale`, the trace items and the type items ([mod.rs:pub use]).

## Invariants
- Indexing stops at `TAINT_MAX_FILES` files and sets `SymbolIndex.truncated = true` ([index.rs:build_index_sync]).
- Only files whose extension maps to the same language `display_name` as the requested `LanguageConfig` are indexed ([index.rs:build_index_sync]).
- `trace` rejects languages with an empty `call_types` list and returns `TraceError::UnsupportedLanguage` ([trace.rs:trace]).
- If the source file cannot be parsed it returns `SourceNotIndexed` or `ParseFailed`. If the sink file cannot be parsed it returns `SinkNotIndexed` or `ParseFailed`. A source line with no enclosing function returns `NoEnclosingFunction` ([trace.rs:trace]).
- BFS stops once `max_paths` paths are found. Frames deeper than `max_depth` are pruned. The search aborts with `truncated_frontier = true` once more than `max_frontier` frames have been queued ([trace.rs:trace]).
- `(FnId, tainted-set)` pairs are visited at most once ([trace.rs:trace]).
- Returned paths are sorted by unresolved hops, then depth, then imprecise bindings (all ascending), and truncated to `max_paths` ([trace.rs:trace]).
- Confidence tiers: 15% or less unresolved is High, 40% or less is Medium, anything else is Low. Zero total calls gives `(0, High)` ([trace.rs:Confidence::from_unresolved_ratio]).
- The sink byte is the first non-whitespace byte on the sink line, so a match does not pick up identifiers from sibling statements ([trace.rs:trace]).

## Side Effects
- Disk reads: walks the directory through `ast::walk_dir` (respects `.gitignore`), parses files through `ast::try_parse_file`, and records mtimes with `std::fs::metadata` ([index.rs:build_index_sync]).
- `is_stale` makes one `metadata` syscall per indexed file ([index.rs:is_stale]).
- `build_index` runs on `tokio::task::spawn_blocking`. A join failure is mapped to `DysonError::tool("taint_trace", ...)` ([index.rs:build_index]).
- `trace` re-reads and parses the source and sink files from disk ([trace.rs:parse]).

## Verification
- Run: `cargo test -p dyson-ast` (from `dyson/`)
