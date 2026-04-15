// ===========================================================================
// Shared tree-sitter node helpers used by AST-walking operations.
//
// Every operation that recurses into a parsed AST (`list_definitions`,
// `extract_definition`, ...) needs the same four bits of logic:
//   - decide whether a node can contain nested definitions
//   - pull a human-readable name out of a definition node
//   - recognise the Elixir `def/defmodule` call convention
//   - clean up the raw tree-sitter kind for display
//
// Keeping the logic in one place ensures `list_definitions` and
// `extract_definition` agree on what counts as a definition and what its
// name is — drift between the two would be a silent bug.
// ===========================================================================

use tree_sitter::Node;

/// Return `true` if an Elixir `call` node is a definition
/// (`def`, `defp`, `defmodule`, `defmacro`, `defprotocol`, `defimpl`).
pub(super) fn is_elixir_definition(node: &Node<'_>, source: &[u8]) -> bool {
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
pub(super) fn is_container_node(kind: &str) -> bool {
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
pub(super) fn extract_definition_name(node: &Node<'_>, source: &[u8]) -> Option<String> {
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
pub(super) fn clean_kind(kind: &str) -> String {
    kind.replace("_item", "")
        .replace("_declaration", "")
        .replace("_definition", "")
        .replace("_specifier", "")
        .replace("_clause", "")
}
