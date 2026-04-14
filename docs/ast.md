# AST-Aware Code Editing

Dyson includes an `ast_edit` tool that uses [tree-sitter](https://tree-sitter.github.io/)
to parse source code into ASTs before making changes.  This lets the agent
rename identifiers across an entire project without accidentally modifying
strings, comments, or substrings of longer names.

A companion `bulk_edit` tool provides simpler glob-based find-and-replace
for cases where AST precision isn't needed.

**Key files:**
- `src/tool/ast_edit/mod.rs` -- `AstEditTool` (agent-only)
- `src/tool/ast_edit/languages.rs` -- Language registry and file parsing
- `src/tool/ast_edit/rename.rs` -- `rename_symbol` implementation
- `src/tool/ast_edit/definitions.rs` -- `list_definitions` implementation
- `src/tool/bulk_edit.rs` -- `BulkEditTool` (agent-only)

---

## Supported Languages

20 tree-sitter grammars covering 19 languages are **statically linked** --
no dynamic loading or network calls.  Each grammar is behind a `LazyLock`
so it only initialises on first use.

| Language   | Extensions                  | Rename | Definitions |
|------------|-----------------------------|--------|-------------|
| Rust       | `.rs`                       | Yes    | Yes         |
| Python     | `.py`, `.pyi`               | Yes    | Yes         |
| JavaScript | `.js`, `.mjs`, `.cjs`, `.jsx` | Yes  | Yes         |
| TypeScript | `.ts`, `.mts`, `.cts`       | Yes    | Yes         |
| TSX        | `.tsx`                      | Yes    | Yes         |
| Go         | `.go`                       | Yes    | Yes         |
| Java       | `.java`                     | Yes    | Yes         |
| C          | `.c`, `.h`                  | Yes    | Yes         |
| C++        | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx` | Yes | Yes   |
| C#         | `.cs`                       | Yes    | Yes         |
| Ruby       | `.rb`                       | Yes    | Yes         |
| Kotlin     | `.kt`, `.kts`               | Yes    | Yes         |
| Swift      | `.swift`                    | Yes    | Yes         |
| Zig        | `.zig`                      | Yes    | Yes         |
| Elixir     | `.ex`, `.exs`               | Yes    | Yes         |
| Erlang     | `.erl`, `.hrl`              | Yes    | Yes         |
| OCaml      | `.ml`, `.mli`               | Yes    | Yes         |
| Haskell    | `.hs`                       | Yes    | Yes         |
| Nix        | `.nix`                      | Yes    | Yes         |
| JSON       | `.json`                     | No     | Yes         |

JSON supports `list_definitions` (enumerates top-level keys) but not
`rename_symbol` since JSON has no identifiers.

---

## Operations

### `rename_symbol`

Renames all occurrences of an identifier across one file or an entire
directory tree.  Only tree-sitter identifier nodes are matched --
strings, comments, and substrings of longer names are never touched.

```json
{
  "path": "src",
  "operation": "rename_symbol",
  "old_name": "Config",
  "new_name": "AppConfig"
}
```

Returns:

```json
{
  "files_modified": 3,
  "occurrences_renamed": 7,
  "files": [
    { "path": "src/config.rs", "occurrences": 4 },
    { "path": "src/main.rs",   "occurrences": 2 },
    { "path": "src/lib.rs",    "occurrences": 1 }
  ]
}
```

How it works:

1. Walk the directory (respecting `.gitignore`), or parse the single file.
2. For each file with a recognised extension, build a tree-sitter AST.
3. Recursively collect identifier nodes whose text matches `old_name` exactly.
4. Sort matches by byte offset descending and replace from end to start
   so earlier offsets stay valid.
5. Write the modified source back to disk.

### `list_definitions`

Lists functions, classes, structs, enums, traits, modules, and other
top-level definitions with line numbers.

```json
{
  "path": "src/agent",
  "operation": "list_definitions"
}
```

Returns:

```json
{
  "definitions": [
    { "kind": "function", "name": "run_inner", "line": 42, "path": "src/agent/mod.rs" },
    { "kind": "struct",   "name": "Agent",     "line": 10, "path": "src/agent/mod.rs" },
    { "kind": "impl",     "name": "impl Agent","line": 80, "path": "src/agent/mod.rs" }
  ]
}
```

Recurses into container nodes (impl blocks, classes, modules, namespaces)
up to depth 2 to find nested definitions.

---

## `bulk_edit` Tool

A simpler complement to `ast_edit` -- plain string find-and-replace
across files matching a glob pattern.  No AST parsing, no language
awareness.  Useful for renaming configuration keys, updating import
paths, or other textual changes.

```json
{
  "pattern": "src/**/*.rs",
  "old_string": "use crate::old_module",
  "new_string": "use crate::new_module",
  "dry_run": false
}
```

- Defaults to `dry_run: true` so the agent must explicitly opt in to writes.
- Respects `.gitignore` via the `ignore` crate.
- Capped at 200 files and 10 MB per file.

---

## Limits

| Limit              | Value  | Rationale                          |
|---------------------|--------|------------------------------------|
| Max file size       | 10 MB  | Prevents OOM on generated files    |
| Max files per op    | 500    | Bounds wall-clock time             |
| Max bulk_edit files | 200    | Tighter bound for text replacement |

---

## Memory Cost

All 20 grammars are statically linked into the binary.  Their parse
tables live in `.rodata` and add roughly 13 MB to the binary's
memory-mapped footprint.  The `LazyLock` wrappers defer the
`LanguageConfig` initialisation, but the underlying grammar data
(e.g. `tree_sitter_cpp::LANGUAGE`) is a constant baked into the
binary at compile time.

Tree-sitter parsers themselves are lightweight -- each `Parser::new()`
call allocates a small working buffer that is freed when the parser
is dropped at the end of each `try_parse_file()` call.  There is no
per-grammar singleton parser; parsers are created and destroyed
per-file.
