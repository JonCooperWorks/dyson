# Security Analysis Tools (`tool/security`)

## Overview
AST-aware security analysis tools used chiefly by the `security_engineer` and `pentester` subagents. Rather than hardcoded vulnerability checks, they expose tree-sitter queries, AST outlines, attack-surface enumeration, and taint tracing over the shared `crate::ast` infrastructure (a facade over `dyson-ast`), plus a static proof-of-concept template generator.

## Interface
- `AstQueryTool` (`ast_query`) - runs tree-sitter S-expression queries across files; `execute_query_string(...)` [ast_query.rs:AstQueryTool, ast_query.rs:execute_query_string]
- `AstDescribeTool` (`ast_describe`) - renders an AST outline of a file or snippet; `describe_source(...)` [ast_describe.rs:AstDescribeTool, ast_describe.rs:describe_source]
- `AttackSurfaceAnalyzerTool` (`attack_surface_analyzer`) - lists external-facing code (HTTP handlers, CLI args, file I/O, network calls, env reads, DB queries) via `SURFACE_PATTERNS` [attack_surface_analyzer.rs:AttackSurfaceAnalyzerTool]
- `TaintTraceTool` (`taint_trace`) - wrapper over `ast::taint::trace` with a per-language `SymbolIndex` cached on `ToolContext` with mtime invalidation [taint_trace.rs:TaintTraceTool]
- `ExploitBuilderTool` (`exploit_builder`) - static template engine producing PoC payloads, curl commands, remediation, or a Nuclei YAML template when a target URL is given [exploit_builder.rs:ExploitBuilderTool]

## Invariants
- Paths are resolved through `ctx.resolve_path`; default scope is `ctx.working_dir` [ast_query.rs:AstQueryTool, attack_surface_analyzer.rs:AttackSurfaceAnalyzerTool, ast_describe.rs:AstDescribeTool]
- Output bounded by `util::MAX_OUTPUT_BYTES` and `ast::MAX_FILES` [ast_query.rs:execute_query_string, attack_surface_analyzer.rs:scan_attack_surface]
- `ast_query`: max 500 matches, 5 s query timeout (`QUERY_TIMEOUT_MICROS = 5_000_000`) [ast_query.rs:MAX_MATCHES, ast_query.rs:QUERY_TIMEOUT_MICROS]
- `attack_surface_analyzer`: max 500 entries [attack_surface_analyzer.rs:MAX_ENTRIES]
- `ast_describe`: default max depth 12, leaf text 64 chars [ast_describe.rs:DEFAULT_MAX_DEPTH, ast_describe.rs:LEAF_TEXT_LIMIT]
- `exploit_builder` only generates text; it does not send requests [exploit_builder.rs:ExploitBuilderTool]

## Side Effects
- Read-only filesystem access to source files [ast_describe.rs:AstDescribeTool, attack_surface_analyzer.rs:scan_attack_surface]
- Query execution runs on `spawn_blocking` [ast_query.rs:AstQueryTool]
- `taint_trace` caches symbol indexes in `ToolContext` [taint_trace.rs:TaintTraceTool]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
