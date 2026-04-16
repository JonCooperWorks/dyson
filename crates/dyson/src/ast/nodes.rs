// ===========================================================================
// Shared tree-sitter node helpers used by AST-walking tools.
//
// Every tool that recurses into a parsed AST needs the same bits of logic:
//   - decide whether a node can contain nested definitions
//   - pull a human-readable name out of a definition node
//   - recognise the Elixir `def/defmodule` call convention
//   - clean up the raw tree-sitter kind for display
//
// Centralising them here keeps `bulk_edit::list_definitions`,
// `read_file`'s symbol extraction, and anything else that walks definitions
// in agreement about what counts and what it's called — drift between
// consumers would be a silent bug.
// ===========================================================================

use tree_sitter::Node;

/// Return `true` if an Elixir `call` node is a definition
/// (`def`, `defp`, `defmodule`, `defmacro`, `defprotocol`, `defimpl`).
pub fn is_elixir_definition(node: &Node<'_>, source: &[u8]) -> bool {
    if let Some(target) = node.child(0)
        && let Ok(text) = std::str::from_utf8(&source[target.start_byte()..target.end_byte()])
    {
        return matches!(
            text,
            "def" | "defp" | "defmodule" | "defmacro" | "defprotocol" | "defimpl"
        );
    }
    false
}

/// Whether a definition node can contain nested definitions.  Used by AST
/// walkers to decide when to recurse beyond the top level.
pub fn is_container_node(kind: &str) -> bool {
    matches!(
        kind,
        "impl_item"
            | "class_definition"
            | "class_declaration"
            | "class_specifier"
            | "namespace_definition"
            | "namespace_declaration"
            | "class"
            | "module"
            | "module_definition"
            | "object_declaration"
            | "call" // Elixir defmodule
    )
}

/// Extract the display name from a definition node, using field names where
/// possible and falling back to the first identifier-like child.
pub fn extract_definition_name(node: &Node<'_>, source: &[u8]) -> Option<String> {
    // Try the "name" field first — most languages use this.
    if let Some(name_node) = node.child_by_field_name("name") {
        let text = &source[name_node.start_byte()..name_node.end_byte()];
        return Some(String::from_utf8_lossy(text).to_string());
    }

    // For Rust impl blocks, synthesize a readable name from the type.
    if node.kind() == "impl_item"
        && let Some(type_node) = node.child_by_field_name("type")
    {
        let text = &source[type_node.start_byte()..type_node.end_byte()];
        return Some(format!("impl {}", String::from_utf8_lossy(text)));
    }

    // For Elixir call nodes (def/defmodule), extract from arguments.
    if node.kind() == "call" {
        return extract_elixir_def_name(node, source);
    }

    // For JSON pair nodes, extract the key.
    if node.kind() == "pair"
        && let Some(key_node) = node.child_by_field_name("key")
    {
        let text = &source[key_node.start_byte()..key_node.end_byte()];
        let key = String::from_utf8_lossy(text).to_string();
        return Some(key.trim_matches('"').to_string());
    }

    // Fallback: first identifier-like child.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "identifier"
                | "type_identifier"
                | "property_identifier"
                | "simple_identifier"
                | "constant"
                | "value_name"
                | "constructor_name"
                | "variable"
                | "atom"
        ) {
            let text = &source[child.start_byte()..child.end_byte()];
            return Some(String::from_utf8_lossy(text).to_string());
        }
    }

    None
}

/// Extract a definition name from an Elixir `def`/`defmodule` call.
fn extract_elixir_def_name(node: &Node<'_>, source: &[u8]) -> Option<String> {
    if let Some(args) = node.child_by_field_name("arguments") {
        let mut cursor = args.walk();
        for child in args.children(&mut cursor) {
            if matches!(child.kind(), "identifier" | "atom" | "alias") {
                let text = &source[child.start_byte()..child.end_byte()];
                return Some(String::from_utf8_lossy(text).to_string());
            }
            if child.kind() == "call"
                && let Some(first) = child.child(0)
            {
                let text = &source[first.start_byte()..first.end_byte()];
                return Some(String::from_utf8_lossy(text).to_string());
            }
        }
    }
    None
}

/// Clean up a raw tree-sitter node kind for display
/// (`function_item` → `function`, `class_declaration` → `class`).
pub fn clean_kind(kind: &str) -> String {
    kind.replace("_item", "")
        .replace("_declaration", "")
        .replace("_definition", "")
        .replace("_specifier", "")
        .replace("_clause", "")
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::try_parse_file;
    use tree_sitter::Tree;

    /// Walk the tree and return the first node whose kind matches `kind`.
    fn first_node_of_kind<'a>(tree: &'a Tree, kind: &str) -> Option<Node<'a>> {
        fn walk<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
            if node.kind() == kind {
                return Some(node);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if let Some(found) = walk(child, kind) {
                    return Some(found);
                }
            }
            None
        }
        walk(tree.root_node(), kind)
    }

    #[test]
    fn clean_kind_strips_known_suffixes() {
        assert_eq!(clean_kind("function_item"), "function");
        assert_eq!(clean_kind("class_declaration"), "class");
        assert_eq!(clean_kind("struct_specifier"), "struct");
        assert_eq!(clean_kind("function_clause"), "function");
        assert_eq!(clean_kind("function_definition"), "function");
        // Unknown kinds pass through unchanged.
        assert_eq!(clean_kind("plain"), "plain");
    }

    #[test]
    fn is_container_node_matches_known_kinds() {
        assert!(is_container_node("impl_item"));
        assert!(is_container_node("class_declaration"));
        assert!(is_container_node("namespace_definition"));
        assert!(is_container_node("call"));
        assert!(is_container_node("module"));

        assert!(!is_container_node("function_item"));
        assert!(!is_container_node("variable_declaration"));
        assert!(!is_container_node(""));
    }

    #[test]
    fn extract_definition_name_rust_function() {
        let src = "fn foo() {}\n";
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lib.rs");
        std::fs::write(&path, src).unwrap();

        let (_, parsed) = try_parse_file(&path, tmp.path(), false).unwrap().unwrap();
        let node = first_node_of_kind(&parsed.tree, "function_item").unwrap();
        let name = extract_definition_name(&node, parsed.source.as_bytes()).unwrap();
        assert_eq!(name, "foo");
    }

    #[test]
    fn extract_definition_name_rust_impl_block() {
        let src = "struct Foo;\nimpl Foo {\n    fn bar(&self) {}\n}\n";
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lib.rs");
        std::fs::write(&path, src).unwrap();

        let (_, parsed) = try_parse_file(&path, tmp.path(), false).unwrap().unwrap();
        let node = first_node_of_kind(&parsed.tree, "impl_item").unwrap();
        let name = extract_definition_name(&node, parsed.source.as_bytes()).unwrap();
        assert_eq!(name, "impl Foo");
    }

    #[test]
    fn extract_definition_name_json_pair() {
        let src = "{\"key\": 42}\n";
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("data.json");
        std::fs::write(&path, src).unwrap();

        let (_, parsed) = try_parse_file(&path, tmp.path(), false).unwrap().unwrap();
        let node = first_node_of_kind(&parsed.tree, "pair").unwrap();
        let name = extract_definition_name(&node, parsed.source.as_bytes()).unwrap();
        assert_eq!(name, "key");
    }

    #[test]
    fn is_elixir_definition_recognises_def_forms() {
        let src = "defmodule M do\n  def a, do: 1\n  defp b, do: 2\n  defmacro c, do: 3\nend\n\nIO.puts(\"hi\")\n";
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.ex");
        std::fs::write(&path, src).unwrap();

        let (_, parsed) = try_parse_file(&path, tmp.path(), false).unwrap().unwrap();
        let source = parsed.source.as_bytes();

        // Collect every `call` node in the tree.
        fn collect_calls<'a>(node: Node<'a>, out: &mut Vec<Node<'a>>) {
            if node.kind() == "call" {
                out.push(node);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_calls(child, out);
            }
        }
        let mut calls = Vec::new();
        collect_calls(parsed.tree.root_node(), &mut calls);

        // Classify each call by the leading keyword text.
        let mut kinds: Vec<&str> = calls
            .iter()
            .filter_map(|c| c.child(0))
            .map(|n| std::str::from_utf8(&source[n.start_byte()..n.end_byte()]).unwrap_or(""))
            .collect();
        kinds.sort();

        // Sanity: we saw the expected call heads.
        assert!(kinds.contains(&"defmodule"));
        assert!(kinds.contains(&"def"));
        assert!(kinds.contains(&"defp"));
        assert!(kinds.contains(&"defmacro"));

        // Every def* call is classified as a definition; `IO.puts` is not.
        for call in &calls {
            let head = call
                .child(0)
                .and_then(|n| std::str::from_utf8(&source[n.start_byte()..n.end_byte()]).ok())
                .unwrap_or("");
            let is_def = matches!(
                head,
                "def" | "defp" | "defmodule" | "defmacro" | "defprotocol" | "defimpl"
            );
            assert_eq!(
                is_elixir_definition(call, source),
                is_def,
                "mismatch for call head {head:?}"
            );
        }
    }
}
