# AST-Aware Code Editing

Dyson's `bulk_edit` tool uses [tree-sitter](https://tree-sitter.github.io/)
to parse source code into ASTs before making changes.  This lets the agent
rename identifiers across an entire project without accidentally modifying
strings, comments, or substrings of longer names.  Non-grammar files
(e.g. `.md`, `.yaml`) fall back to word-boundary text replacement, so a
single rename can sweep source code, docs, and config together.

**Key files:**
- `src/tool/bulk_edit/mod.rs` -- `BulkEditTool` (agent-only), dispatches operations
- `src/tool/bulk_edit/languages.rs` -- Language registry and file parsing
- `src/tool/bulk_edit/rename.rs` -- `rename_symbol` (AST + text fallback)
- `src/tool/bulk_edit/find_replace.rs` -- `find_replace` (plain text / regex)
- `src/tool/bulk_edit/definitions.rs` -- `list_definitions` (AST only)

---

## Supported Languages

20 tree-sitter grammars covering 19 languages are **statically linked** --
no dynamic loading or network calls.  Each grammar is behind a `LazyLock`
so it only initialises on first use.

| Language   | Extensions                  | AST rename | Definitions |
|------------|-----------------------------|------------|-------------|
| Rust       | `.rs`                       | Yes        | Yes         |
| Python     | `.py`, `.pyi`               | Yes        | Yes         |
| JavaScript | `.js`, `.mjs`, `.cjs`, `.jsx` | Yes      | Yes         |
| TypeScript | `.ts`, `.mts`, `.cts`       | Yes        | Yes         |
| TSX        | `.tsx`                      | Yes        | Yes         |
| Go         | `.go`                       | Yes        | Yes         |
| Java       | `.java`                     | Yes        | Yes         |
| C          | `.c`, `.h`                  | Yes        | Yes         |
| C++        | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx` | Yes | Yes    |
| C#         | `.cs`                       | Yes        | Yes         |
| Ruby       | `.rb`                       | Yes        | Yes         |
| Kotlin     | `.kt`, `.kts`               | Yes        | Yes         |
| Swift      | `.swift`                    | Yes        | Yes         |
| Zig        | `.zig`                      | Yes        | Yes         |
| Elixir     | `.ex`, `.exs`               | Yes        | Yes         |
| Erlang     | `.erl`, `.hrl`              | Yes        | Yes         |
| OCaml      | `.ml`, `.mli`               | Yes        | Yes         |
| Haskell    | `.hs`                       | Yes        | Yes         |
| Nix        | `.nix`                      | Yes        | Yes         |
| JSON       | `.json`                     | No         | Yes         |

Any other file extension (`.md`, `.yaml`, `.toml`, `.txt`, `.html`, ...)
is handled by `rename_symbol`'s text fallback -- see below.

---

## Operations

### `rename_symbol`

Renames all occurrences of an identifier across one file or an entire
directory tree.  For each file:

- If the extension has a tree-sitter grammar with identifier nodes, only
  identifier AST nodes matching `old_name` exactly are renamed.  Strings,
  comments, doc-comments, and substrings are never touched.
- Otherwise, the file is rewritten with a word-boundary text replace:
  the chars on each side of the match must be non-alphanumeric and
  non-underscore.  This prevents `Config` from matching inside
  `ConfigManager` in text mode.

```json
{
  "operation": "rename_symbol",
  "path": "src",
  "old_name": "Config",
  "new_name": "AppConfig"
}
```

Optional: `"dry_run": true` previews matches without writing.

Returns (per-file `method` shows which path was taken):

```json
{
  "files_modified": 3,
  "occurrences_renamed": 7,
  "dry_run": false,
  "files": [
    { "path": "src/config.rs",     "count": 4, "method": "ast" },
    { "path": "src/main.rs",       "count": 2, "method": "ast" },
    { "path": "docs/config.md",    "count": 1, "method": "text" }
  ]
}
```

### `find_replace`

Plain text (or regex) find-and-replace across files.  No AST, no word
boundary -- exact substring match.  Use this for URL changes, import
path rewrites, license headers, or any non-symbol replacement.

```json
{
  "operation": "find_replace",
  "path": ".",
  "pattern": "http://",
  "replacement": "https://"
}
```

With regex (capture groups via `$1`, `$2`, ...):

```json
{
  "operation": "find_replace",
  "path": "src",
  "pattern": "(\\w+)_old",
  "replacement": "${1}_new",
  "regex": true
}
```

Optional: `"dry_run": true` previews without writing.

Returns:

```json
{
  "files_modified": 4,
  "occurrences_replaced": 12,
  "dry_run": false,
  "files": [
    { "path": "src/main.rs", "count": 3 },
    { "path": "README.md",   "count": 2 }
  ]
}
```

### `list_definitions`

Lists functions, classes, structs, enums, traits, modules, and other
top-level definitions with line numbers.  AST-only -- files without a
registered grammar are silently skipped.

```json
{
  "operation": "list_definitions",
  "path": "src/agent"
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

## Limits

| Limit                | Value  | Rationale                                       |
|----------------------|--------|-------------------------------------------------|
| Max file size        | 10 MB  | Prevents OOM on generated files                 |
| Max files (AST ops)  | 500    | Bounds wall-clock time for rename/definitions   |
| Max files (find_replace) | 200 | Tighter bound: text replace can touch many more files |

Binary / non-UTF-8 files are always skipped silently.  All directory
walks respect `.gitignore` via the `ignore` crate.

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
