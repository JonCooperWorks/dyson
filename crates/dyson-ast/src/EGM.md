# dyson-ast src

## Overview
This is shared tree-sitter infrastructure for Dyson's code-aware tools. It contains the grammar registry (`languages`), helpers for inspecting definition nodes (`nodes`), a few cross-cutting search primitives, and the `taint` reachability submodule. Per the module comment, consumers go through this crate so that limits, grammar coverage and tree-walking rules stay the same across `bulk_edit`, `read_file` and `search_files` ([lib.rs:module comment]).

## Interface
- Modules: `pub mod languages`, `pub mod nodes`, `pub mod taint` ([lib.rs]).
- `taint`: the taint index and BFS trace. See [taint/EGM.md](./taint/EGM.md).
- Re-exports from `languages` ([lib.rs:pub use languages]):
  - `LanguageConfig { language, identifier_types, definition_types, call_types, display_name, definitions_are_calls }` ([languages.rs:LanguageConfig]).
  - `ParsedFile { tree, source, rel_path }` ([languages.rs:ParsedFile]).
  - `MAX_FILE_SIZE` (10 MiB) and `MAX_FILES` (500) ([languages.rs:MAX_FILE_SIZE], [languages.rs:MAX_FILES]).
  - `try_parse_file(path, working_dir_canon, require_identifiers) -> Result<Option<(&LanguageConfig, ParsedFile)>>` ([languages.rs:try_parse_file]).
  - `walk_dir` and `walk_dir_filtered(dir, include_glob)` ([languages.rs:walk_dir_filtered]).
  - `config_for_extension`, `config_for_glob`, `config_for_language_name` and `supported_language_names` ([languages.rs]).
- Search primitives ([lib.rs]):
  - `find_identifier_positions(tree, source, target_name, id_types) -> Vec<(usize, usize)>`.
  - `find_word_boundary_matches(haystack, needle)`.
  - `find_definitions_by_name(parsed, config, target_name, kind_filter) -> Vec<DefinitionMatch>`.
  - `find_enclosing_function(node, config, source) -> Option<Node>`.
- `DefinitionMatch { kind, name, line, start_byte, end_byte }` ([lib.rs:DefinitionMatch]).
- Node helpers ([nodes.rs]):
  - `is_elixir_definition`, which matches `def`, `defp`, `defmodule`, `defmacro`, `defprotocol` and `defimpl`.
  - `is_container_node`.
  - `extract_definition_name`, which tries the `name` field, then Rust `impl` types, Elixir call arguments, JSON `pair` keys, and finally the first identifier-like child.
  - `clean_kind`, which strips the `_item`, `_declaration`, `_definition`, `_specifier` and `_clause` suffixes.

## Invariants
- `try_parse_file` returns `Ok(None)` (skip, not an error) in these cases: no or unknown extension, `require_identifiers` set for a grammar with empty `identifier_types`, failed `metadata`, a file larger than `MAX_FILE_SIZE`, a read or UTF-8 failure, or a parse failure. Only grammar setup failure is an `Err` ([languages.rs:try_parse_file]).
- `rel_path` is `path` relative to `working_dir_canon`, falling back to the full path ([languages.rs:try_parse_file]).
- `config_for_extension` delegates to `config_for_language_name`, so there is one alias table. Lookup is case-insensitive ([languages.rs:config_for_extension], [languages.rs:config_for_language_name]).
- `supported_language_names` lists 20 grammar names in a fixed order: rust through json ([languages.rs:supported_language_names]).
- `config_for_glob` uses the trailing `.ext` only, so brace globs like `*.{ts,tsx}` resolve to `None` ([languages.rs:config_for_glob]).
- `walk_dir_filtered` includes hidden files and respects `.gitignore` and the global gitignore. An invalid include glob is ignored and the walk runs unfiltered ([languages.rs:walk_dir_filtered]).
- `find_word_boundary_matches` treats ASCII alphanumerics, `_` and every non-ASCII byte as identifier bytes. An empty needle returns no matches ([lib.rs:find_word_boundary_matches], [lib.rs:is_ident_byte]).
- Elixir `call` nodes count as definitions only when `is_elixir_definition` is true and `definitions_are_calls` is set ([lib.rs:walk_defs], [lib.rs:find_enclosing_function]).

## Side Effects
- Disk reads in `try_parse_file` (`std::fs::metadata`, `std::fs::read_to_string`) ([languages.rs:try_parse_file]).
- Directory traversal through `ignore::WalkBuilder` ([languages.rs:walk_dir_filtered]).
- Global state: one `static LazyLock<LanguageConfig>` per grammar, such as `RUST`, `PYTHON` and `JSON`. All grammars are statically linked ([languages.rs]).

## Verification
- Run: `cargo test -p dyson-ast` (from `dyson/`)
