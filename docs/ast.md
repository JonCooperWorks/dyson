# AST-Aware Code Editing and Reading

Renaming a symbol with a plain text search-and-replace is a good way to corrupt
a codebase.  `Config` appears inside `ConfigManager`, inside string literals,
inside comments, and inside doc examples that were never meant to change.  The
fix is to parse the file first and only rewrite the nodes that the language
actually considers identifiers.

Dyson uses [tree-sitter](https://tree-sitter.github.io/) to parse source into
an AST, then walks that AST to find identifier nodes matching the target name.
Strings, comments, and substrings of longer names are left alone.  Files that
have no grammar registered fall through to a narrower text-replacement path
(see ["Why the text fallback exists"](#why-the-text-fallback-exists) below),
so a single rename can sweep source, docs, and config in one pass.

The same machinery powers symbol-aware reads (`read_file` with `symbol: ...`)
and symbol-aware searches (`search_files` with `ast: true`), so the agent can
extract one function out of a 5k-line file or audit every usage of a symbol
without scanning the wrong things.

**Key files:**
- `src/tool/ast/mod.rs` -- shared AST primitives (`find_identifier_positions`,
  `find_word_boundary_matches`, `find_definitions_by_name`); consumed by
  `bulk_edit`, `read_file`, and `search_files`
- `src/tool/ast/languages.rs` -- language registry and file parsing
- `src/tool/ast/nodes.rs` -- node-info helpers (definition name extraction,
  container detection, kind cleanup)
- `src/tool/bulk_edit/mod.rs` -- `BulkEditTool` (agent-only), dispatches ops
- `src/tool/bulk_edit/rename.rs` -- `rename_symbol` (AST + text fallback)
- `src/tool/bulk_edit/find_replace.rs` -- `find_replace` (plain text / regex)
- `src/tool/bulk_edit/definitions.rs` -- `list_definitions` (AST only)
- `src/tool/read_file.rs` -- adds `symbol` extraction mode
- `src/tool/search_files.rs` -- adds `ast: true` identifier-search mode

---

## Supported Languages

The grammar set was picked to cover the working set of codebases Dyson is
likely to be dropped into -- systems work (Rust, C, C++, Zig), mainstream
backend (Go, Java, Kotlin, C#, Ruby), the scripting trio
(Python, JavaScript/TypeScript), functional languages
(OCaml, Haskell, Elixir, Erlang), and the config layer (Nix, JSON).  Every
grammar is **statically linked** into the binary.  There is no runtime
download, no plugin directory, no ABI-versioned `.so` files to keep in sync
with the parser, and therefore no third attack surface to secure.

Each grammar is guarded by a `LazyLock` so its `LanguageConfig` only
initialises the first time that language is touched.  The parse tables
themselves live in `.rodata` and are always present.

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

Anything outside this list -- `.md`, `.yaml`, `.toml`, `.txt`, `.html`,
Dockerfiles, shell scripts -- is handled by `rename_symbol`'s text fallback.

---

## Why the text fallback exists

A real refactor rarely stops at source files.  Rename a public type and you
also need to touch:

- Markdown docs and README sections that mention the symbol by name
- YAML, TOML, and Nix config keys that reference it
- Makefiles, shell scripts, and Dockerfiles that invoke it
- Generated or hand-written `.txt`, `.html`, or fixture files that embed it

Shipping a tree-sitter grammar for each of those is impractical.  Grammars are
heavy, some of these formats (Markdown, plain text) have no notion of an
"identifier" in the AST sense, and the overlap in binary size would be
significant for very little gain.

The text fallback closes that gap with one rule: a match only counts if the
characters on both sides are non-alphanumeric and non-underscore.  That
word-boundary check is enough to keep `Config` inside `ConfigManager`
untouched in a Markdown file while still catching `Config` in prose, a YAML
key, a `docker build --build-arg Config=...` line, or a `.env` template.

The result is that `rename_symbol` is a single-shot, cross-cutting refactor
rather than "AST for code, manual `sed` for everything else."  Source is
rewritten with AST precision; text is rewritten with word-boundary safety;
binary and non-UTF-8 files are skipped; and the tool's output tells you
exactly which path ran for each file.

---

## Operations

### `rename_symbol`

Renames all occurrences of an identifier across one file or an entire
directory tree.  For each file:

- If the extension has a tree-sitter grammar with identifier nodes, only
  identifier AST nodes matching `old_name` exactly are renamed.  Strings,
  comments, doc-comments, and substrings are never touched.
- Otherwise, the file is rewritten with the word-boundary text replace
  described above.

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

## Reading and Searching

The same AST infrastructure also powers two read-side modes on the existing
`read_file` and `search_files` tools.  The point is the same: when the agent
is reading or auditing code, plain text grep is risky in the same ways that
plain text rename is.

### `read_file` symbol mode

```json
{
  "file_path": "src/agent/mod.rs",
  "symbol": "Agent"
}
```

Parses the file with tree-sitter and returns the source of every definition
named `Agent`, with kind and line annotations.  Optional `symbol_kind`
(`"function"`, `"struct"`, `"class"`, ...) disambiguates when the same name
has multiple definitions.

Behaviour:

- Walks the entire AST so methods inside `impl Foo` / `class Foo:` are reachable.
- Returns one or more spans separated by blank lines, each prefixed with a
  `// path:line (kind)` header.
- AST-only -- unsupported extensions return a clear error rather than
  silently degrading to a text grep that would hit comments and strings.

Use this instead of `read_file` + `offset`/`limit` when you know the symbol
you want.  Saves substantial context on large files.

### `search_files` AST mode

```json
{
  "pattern": "Config",
  "ast": true
}
```

In AST mode, `pattern` is a literal identifier name (not a regex):

- Files with a grammar: only identifier AST nodes match, with the same
  precision guarantee as `rename_symbol` -- `Config` does not match inside
  `ConfigManager`, `"Config"`, or `// Config`.
- Files without a grammar: word-boundary literal match (Markdown, YAML,
  configs are still searched).

Output uses the same `path:line: text` format as regex mode.  Use this to
audit every usage of a symbol before/after a rename, or to find call sites
that a regex grep would either miss (typed as `Self::Foo`) or over-count
(spelled inside an unrelated docstring).

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

Carrying 20 grammars for 19 languages is not free.  The parse tables are
baked into the binary at compile time and add roughly **35 MB** to its
memory-mapped footprint -- they sit in `.rodata` and are always present
whether or not a given language is used in a session.  The `LazyLock`
wrappers only defer the per-language `LanguageConfig` setup; the
underlying grammar constants (e.g. `tree_sitter_cpp::LANGUAGE`) are
unconditionally linked.

The tradeoff is deliberate.  35 MB of static data buys correctness-
preserving renames across every language a user is realistically going
to hand Dyson, with no plugin download step, no grammar ABI version skew
to track, no supply-chain surface beyond the crates already audited at
build time, and no startup cost past the first parse per language.

Parsers themselves are cheap.  `Parser::new()` allocates a small working
buffer that is freed when the parser is dropped at the end of each
`try_parse_file()` call.  There is no per-grammar singleton parser --
parsers are created and destroyed per-file.
