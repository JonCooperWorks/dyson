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

use tree_sitter::{Node, Parser};

use crate::error::Result;
use crate::tool::ToolOutput;

use super::languages::{self, LanguageConfig};

/// Maximum file size for AST parsing (10 MB).
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Maximum number of files to scan.
const MAX_FILES: usize = 500;

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
        let mut builder = ignore::WalkBuilder::new(resolved_path);
        builder.hidden(false);
        builder.git_ignore(true);
        builder.git_global(true);

        for entry in builder.build().flatten() {
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
/// Returns `true` if the file was actually processed (had a supported extension).
fn process_file(
    path: &Path,
    working_dir_canon: &Path,
    defs: &mut Vec<serde_json::Value>,
) -> Result<bool> {
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e,
        None => return Ok(false),
    };
    let config = match languages::config_for_extension(ext) {
        Some(c) => c,
        None => return Ok(false),
    };
    if config.definition_types.is_empty() {
        return Ok(false);
    }

    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(false),
    };
    if metadata.len() > MAX_FILE_SIZE {
        return Ok(false);
    }

    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };

    let mut parser = Parser::new();
    parser
        .set_language(&config.language)
        .map_err(|e| crate::error::DysonError::tool("ast_edit", format!("parser setup: {e}")))?;

    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => return Ok(false),
    };

    let rel_path = path
        .strip_prefix(working_dir_canon)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string());

    let source_bytes = source.as_bytes();
    let root = tree.root_node();
    let mut cursor = root.walk();

    for child in root.children(&mut cursor) {
        collect_definitions_recursive(child, source_bytes, config, &rel_path, defs, 0);
    }

    Ok(true)
}

/// Collect definitions, recursing into impl/class/module blocks up to `depth` 2.
fn collect_definitions_recursive(
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

    // For Elixir, only treat `call` nodes as definitions when they are
    // def/defp/defmodule/defmacro.
    if config.display_name == "Elixir"
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
            collect_definitions_recursive(child, source, config, rel_path, results, depth + 1);
        }
    }
}

/// Check if an Elixir `call` node is a definition (def/defp/defmodule/defmacro).
fn is_elixir_definition(node: &Node<'_>, source: &[u8]) -> bool {
    // The first child of a call is the function being called.
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

    // For Elixir call nodes (def/defmodule), extract the second argument.
    if node.kind() == "call" {
        return extract_elixir_def_name(node, source);
    }

    // For JSON pair nodes, extract the key.
    if node.kind() == "pair"
        && let Some(key_node) = node.child_by_field_name("key")
    {
        let text = &source[key_node.start_byte()..key_node.end_byte()];
        let key = String::from_utf8_lossy(text).to_string();
        // Strip surrounding quotes.
        return Some(key.trim_matches('"').to_string());
    }

    // Fallback: first identifier-like child.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if kind == "identifier"
            || kind == "type_identifier"
            || kind == "property_identifier"
            || kind == "simple_identifier"
            || kind == "constant"
            || kind == "value_name"
            || kind == "constructor_name"
            || kind == "variable"
            || kind == "atom"
        {
            let text = &source[child.start_byte()..child.end_byte()];
            return Some(String::from_utf8_lossy(text).to_string());
        }
    }

    None
}

/// Extract definition name from an Elixir def/defmodule call.
fn extract_elixir_def_name(node: &Node<'_>, source: &[u8]) -> Option<String> {
    // In Elixir tree-sitter: call has arguments child which contains
    // the function/module name as the first argument.
    if let Some(args) = node.child_by_field_name("arguments") {
        let mut cursor = args.walk();
        for child in args.children(&mut cursor) {
            let kind = child.kind();
            if kind == "identifier" || kind == "atom" || kind == "alias" {
                let text = &source[child.start_byte()..child.end_byte()];
                return Some(String::from_utf8_lossy(text).to_string());
            }
            // For defmodule, the first argument is often an alias (e.g., MyModule).
            if kind == "call" {
                // Nested call — try its first child.
                if let Some(first) = child.child(0) {
                    let text = &source[first.start_byte()..first.end_byte()];
                    return Some(String::from_utf8_lossy(text).to_string());
                }
            }
        }
    }
    None
}

/// Clean up node kind for display (e.g., "function_item" → "function").
fn clean_kind(kind: &str) -> String {
    kind.replace("_item", "")
        .replace("_declaration", "")
        .replace("_definition", "")
        .replace("_specifier", "")
        .replace("_clause", "")
}
