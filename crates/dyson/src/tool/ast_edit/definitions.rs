// ===========================================================================
// list_definitions — extract top-level definitions from source files.
//
// Walks tree-sitter ASTs to find function, class, struct, enum, trait,
// module, and other definition nodes.  Returns structured JSON output
// with kind, name, line number, and file path.
//
// Supports single files and recursive directory walks (with .gitignore).
// ===========================================================================

use std::path::Path;

use tree_sitter::Node;

use crate::error::Result;
use crate::tool::ToolOutput;

use super::languages::{self, LanguageConfig, MAX_FILES};

/// List definitions in the given path (file or directory).
///
/// Returns a JSON array of definitions with kind, name, line, and path.
pub fn list_definitions(resolved_path: &Path, working_dir: &Path) -> Result<ToolOutput> {
    let working_dir_canon = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());

    let mut all_defs: Vec<serde_json::Value> = Vec::new();
    let mut files_scanned = 0usize;

    if resolved_path.is_file() {
        process_file(resolved_path, &working_dir_canon, &mut all_defs)?;
    } else if resolved_path.is_dir() {
        for entry in languages::walk_dir(resolved_path).flatten() {
            if files_scanned >= MAX_FILES {
                break;
            }
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if process_file(path, &working_dir_canon, &mut all_defs)? {
                files_scanned += 1;
            }
        }
    } else {
        return Ok(ToolOutput::error(format!(
            "path '{}' is not a file or directory",
            resolved_path.display()
        )));
    }

    let json = serde_json::json!({ "definitions": all_defs });
    Ok(ToolOutput::success(json.to_string()))
}

/// Process a single file, appending definitions to `defs`.
/// Returns `true` if the file was actually processed.
fn process_file(
    path: &Path,
    working_dir_canon: &Path,
    defs: &mut Vec<serde_json::Value>,
) -> Result<bool> {
    let (config, parsed) =
        match languages::try_parse_file(path, working_dir_canon, false)? {
            Some(pair) => pair,
            None => return Ok(false),
        };

    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        collect_definitions(child, parsed.source.as_bytes(), config, &parsed.rel_path, defs, 0);
    }

    Ok(true)
}

/// Collect definitions, recursing into impl/class/module blocks up to depth 2.
fn collect_definitions(
    node: Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    rel_path: &str,
    results: &mut Vec<serde_json::Value>,
    depth: usize,
) {
    if depth > 2 {
        return;
    }

    if !config.definition_types.contains(&node.kind()) {
        return;
    }

    // Elixir represents definitions as `call` nodes wrapping def/defmodule.
    // Skip call nodes that aren't actual definitions.
    if config.definitions_are_calls
        && node.kind() == "call"
        && !is_elixir_definition(&node, source)
    {
        return;
    }

    let name = extract_definition_name(&node, source)
        .unwrap_or_else(|| "<anonymous>".to_string());
    let line = node.start_position().row + 1;
    let kind = clean_kind(node.kind());

    results.push(serde_json::json!({
        "kind": kind,
        "name": name,
        "line": line,
        "path": rel_path,
    }));

    // Recurse into container blocks to find nested definitions.
    if is_container_node(node.kind()) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_definitions(child, source, config, rel_path, results, depth + 1);
        }
    }
}

/// Check if an Elixir `call` node is a definition (def/defp/defmodule/defmacro).
fn is_elixir_definition(node: &Node<'_>, source: &[u8]) -> bool {
    if let Some(target) = node.child(0)
        && let Ok(text) = std::str::from_utf8(&source[target.start_byte()..target.end_byte()])
    {
        return matches!(text, "def" | "defp" | "defmodule" | "defmacro" | "defprotocol" | "defimpl");
    }
    false
}

/// Whether a definition node can contain nested definitions.
fn is_container_node(kind: &str) -> bool {
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

/// Extract the name from a definition node.
fn extract_definition_name(node: &Node<'_>, source: &[u8]) -> Option<String> {
    // Try the "name" field first — most languages use this.
    if let Some(name_node) = node.child_by_field_name("name") {
        let text = &source[name_node.start_byte()..name_node.end_byte()];
        return Some(String::from_utf8_lossy(text).to_string());
    }

    // For Rust impl blocks, try the "type" field.
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

/// Extract definition name from an Elixir def/defmodule call.
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

/// Clean up node kind for display (e.g., "function_item" -> "function").
fn clean_kind(kind: &str) -> String {
    kind.replace("_item", "")
        .replace("_declaration", "")
        .replace("_definition", "")
        .replace("_specifier", "")
        .replace("_clause", "")
}
