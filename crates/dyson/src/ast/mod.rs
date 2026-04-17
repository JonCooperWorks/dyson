// ===========================================================================
// AST — shared tree-sitter infrastructure for code-aware tools.
//
// This module owns the grammar registry (`languages`), the node-inspection
// helpers (`nodes`), and the small set of cross-cutting primitives that
// multiple tools need:
//   - `find_identifier_positions` — collect identifier node spans matching
//     a name (used by rename_symbol, search_files AST mode).
//   - `find_word_boundary_matches` — safe literal-string match for files
//     without a grammar (rename_symbol fallback, search_files AST mode).
//   - `find_definitions_by_name` — locate definition nodes with a given
//     name + optional kind filter (used by read_file symbol extraction).
//
// Every consumer goes through this module rather than reaching into a
// specific tool's internals.  Limits, grammar coverage, and walking rules
// are therefore uniform across `bulk_edit`, `read_file`, and `search_files`.
// ===========================================================================

pub mod languages;
pub mod nodes;
pub mod taint;

pub use languages::{
    LanguageConfig, MAX_FILE_SIZE, MAX_FILES, ParsedFile, config_for_extension,
    config_for_glob, config_for_language_name, try_parse_file, walk_dir,
};

use tree_sitter::{Node, Tree};

/// A single definition node located by name (and optionally kind).
#[derive(Debug, Clone)]
pub struct DefinitionMatch {
    pub kind: String,
    pub name: String,
    pub line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// Recursively collect every identifier node whose text equals `target_name`.
///
/// Only nodes whose kind appears in `id_types` are considered — this is how
/// we ignore string literals, comments, and other non-identifier text that
/// happens to spell the same characters.  Returned spans are `(start_byte,
/// end_byte)` in the order encountered during the walk.
pub fn find_identifier_positions(
    tree: &Tree,
    source: &[u8],
    target_name: &str,
    id_types: &[&str],
) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    collect(tree.root_node(), source, target_name, id_types, &mut out);
    out
}

fn collect(
    node: Node<'_>,
    source: &[u8],
    target_name: &str,
    id_types: &[&str],
    out: &mut Vec<(usize, usize)>,
) {
    if id_types.contains(&node.kind())
        && let Ok(text) = std::str::from_utf8(&source[node.start_byte()..node.end_byte()])
        && text == target_name
    {
        out.push((node.start_byte(), node.end_byte()));
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, target_name, id_types, out);
    }
}

/// Find every substring match of `needle` in `haystack` where the bytes on
/// both sides are non-identifier chars (non-alphanumeric, non-underscore)
/// or string boundaries.  This is the "safe" text match used for files
/// without a tree-sitter grammar: it prevents `Config` from matching inside
/// `ConfigManager` while still catching `Config` in prose, YAML keys, CLI
/// flags, and similar.
///
/// Non-ASCII bytes are treated as identifier bytes — conservative, so a
/// match adjacent to a multi-byte codepoint is skipped rather than risk a
/// bad replacement.
pub fn find_word_boundary_matches(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    let bytes = haystack.as_bytes();
    let nbytes = needle.as_bytes();
    let nlen = nbytes.len();

    let mut i = 0usize;
    while i + nlen <= bytes.len() {
        if &bytes[i..i + nlen] == nbytes {
            let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
            let after_ok = i + nlen == bytes.len() || !is_ident_byte(bytes[i + nlen]);
            if before_ok && after_ok {
                out.push((i, i + nlen));
                i += nlen;
                continue;
            }
        }
        i += 1;
    }
    out
}

const fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || !b.is_ascii()
}

/// Find every definition node in `parsed` whose name matches `target_name`,
/// optionally filtered to a specific kind (`function`, `class`, `struct`,
/// etc. — the cleaned form from [`nodes::clean_kind`]).
///
/// Walks the entire tree so methods defined inside `impl`/`class`/`module`
/// blocks are reachable.  Identifying a single named definition is cheap
/// enough that bounding the walk by depth would only add complexity for no
/// real gain.
pub fn find_definitions_by_name(
    parsed: &ParsedFile,
    config: &LanguageConfig,
    target_name: &str,
    kind_filter: Option<&str>,
) -> Vec<DefinitionMatch> {
    let mut out = Vec::new();
    walk_defs(
        parsed.tree.root_node(),
        parsed.source.as_bytes(),
        config,
        target_name,
        kind_filter,
        &mut out,
    );
    out
}

fn walk_defs(
    node: Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    target_name: &str,
    kind_filter: Option<&str>,
    out: &mut Vec<DefinitionMatch>,
) {
    if config.definition_types.contains(&node.kind()) {
        // Elixir wraps definitions in `call` nodes; filter out non-definition calls.
        let elixir_skip = config.definitions_are_calls
            && node.kind() == "call"
            && !nodes::is_elixir_definition(&node, source);
        if !elixir_skip {
            let name = nodes::extract_definition_name(&node, source).unwrap_or_default();
            let kind = nodes::clean_kind(node.kind());
            if name == target_name && kind_filter.is_none_or(|k| k == kind) {
                out.push(DefinitionMatch {
                    kind,
                    name,
                    line: node.start_position().row + 1,
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
                });
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_defs(child, source, config, target_name, kind_filter, out);
    }
}

/// Walk up from `node` to the nearest ancestor whose kind is one of
/// `config.definition_types` (function / method / class / module — whatever
/// the language considers a definition).  Returns the ancestor node, so
/// callers can query its body range, extract its parameters, or derive a
/// name via [`nodes::extract_definition_name`].
///
/// Returns `None` when `node` lies outside any definition (module-level
/// top-level code, e.g. Python scripts without a `def`).
pub fn find_enclosing_function<'a>(
    node: Node<'a>,
    config: &LanguageConfig,
    source: &[u8],
) -> Option<Node<'a>> {
    let mut current = Some(node);
    while let Some(cur) = current {
        if config.definition_types.contains(&cur.kind()) {
            let elixir_skip = config.definitions_are_calls
                && cur.kind() == "call"
                && !nodes::is_elixir_definition(&cur, source);
            if !elixir_skip {
                return Some(cur);
            }
        }
        current = cur.parent();
    }
    None
}

// ===========================================================================
// Tests — exercise the utility functions directly.  The module-specific
// tests (language registry, node helpers) live with their submodules.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_boundary_basic() {
        let m = find_word_boundary_matches("Config is ConfigManager", "Config");
        assert_eq!(m, vec![(0, 6)]);
    }

    #[test]
    fn word_boundary_string_boundaries() {
        assert_eq!(find_word_boundary_matches("foo", "foo"), vec![(0, 3)]);
        assert!(find_word_boundary_matches("_foo", "foo").is_empty());
        assert!(find_word_boundary_matches("foo_", "foo").is_empty());
    }

    #[test]
    fn identifier_positions_ignore_strings_and_comments() {
        let src = "fn target() {}\nfn other() { target(); }\n// target\nlet s = \"target\";\n";
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lib.rs");
        std::fs::write(&path, src).unwrap();

        let (config, parsed) = try_parse_file(&path, tmp.path(), true).unwrap().unwrap();
        let positions = find_identifier_positions(
            &parsed.tree,
            parsed.source.as_bytes(),
            "target",
            config.identifier_types,
        );
        assert_eq!(positions.len(), 2);
    }

    #[test]
    fn definitions_by_name_finds_nested_method() {
        let src = "struct Foo;\nimpl Foo {\n    fn outer() {}\n    fn target(&self) {}\n}\n";
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lib.rs");
        std::fs::write(&path, src).unwrap();

        let (config, parsed) = try_parse_file(&path, tmp.path(), false).unwrap().unwrap();
        let matches = find_definitions_by_name(&parsed, config, "target", None);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].kind, "function");
        assert_eq!(matches[0].name, "target");

        let body =
            &parsed.source[matches[0].start_byte..matches[0].end_byte];
        assert!(body.contains("fn target(&self)"));
    }

    #[test]
    fn definitions_by_name_kind_filter() {
        let src = "fn target() {}\nstruct target { x: i32 }\n";
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lib.rs");
        std::fs::write(&path, src).unwrap();

        let (config, parsed) = try_parse_file(&path, tmp.path(), false).unwrap().unwrap();
        let both = find_definitions_by_name(&parsed, config, "target", None);
        assert_eq!(both.len(), 2);

        let struct_only = find_definitions_by_name(&parsed, config, "target", Some("struct"));
        assert_eq!(struct_only.len(), 1);
        assert_eq!(struct_only[0].kind, "struct");
    }
}
