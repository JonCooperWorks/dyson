// ===========================================================================
// list_definitions — extract top-level definitions from source files.
//
// Walks tree-sitter ASTs to find function, class, struct, enum, trait,
// module, and other definition nodes.  Returns structured JSON output
// with kind, name, line number, and file path.
//
// AST only: files without a registered grammar are silently skipped.
// ===========================================================================

use std::path::Path;

use tree_sitter::Node;

use crate::error::Result;
use crate::tool::ToolOutput;

use crate::ast::nodes::{
    clean_kind, extract_definition_name, is_container_node, is_elixir_definition,
};
use crate::ast::{self, LanguageConfig, MAX_FILES};

/// List definitions in the given path (file or directory).
///
/// Returns a JSON object with a `definitions` array.
pub fn list_definitions(resolved_path: &Path, working_dir: &Path) -> Result<ToolOutput> {
    let working_dir_canon = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());

    let mut all_defs: Vec<serde_json::Value> = Vec::new();
    let mut files_scanned = 0usize;

    if resolved_path.is_file() {
        process_file(resolved_path, &working_dir_canon, &mut all_defs)?;
    } else if resolved_path.is_dir() {
        for entry in ast::walk_dir(resolved_path).flatten() {
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
    let (config, parsed) = match ast::try_parse_file(path, working_dir_canon, false)? {
        Some(pair) => pair,
        None => return Ok(false),
    };

    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        collect_definitions(
            child,
            parsed.source.as_bytes(),
            config,
            &parsed.rel_path,
            defs,
            0,
        );
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
    if config.definitions_are_calls && node.kind() == "call" && !is_elixir_definition(&node, source)
    {
        return;
    }

    let name = extract_definition_name(&node, source).unwrap_or_else(|| "<anonymous>".to_string());
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

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_definitions_python() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("app.py"),
            "def hello():\n    pass\n\n\
             class MyClass:\n    def method(self):\n        pass\n\n\
             def goodbye():\n    pass\n",
        )
        .unwrap();

        let output = list_definitions(&tmp.path().join("app.py"), tmp.path()).unwrap();
        assert!(!output.is_error, "error: {}", output.content);

        let json: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        let defs = json["definitions"].as_array().unwrap();
        assert!(
            defs.len() >= 3,
            "expected at least 3 definitions, got {}",
            defs.len()
        );

        let names: Vec<&str> = defs.iter().filter_map(|d| d["name"].as_str()).collect();
        assert!(names.contains(&"hello"));
        assert!(names.contains(&"MyClass"));
        assert!(names.contains(&"goodbye"));
    }

    #[tokio::test]
    async fn list_definitions_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn foo() {}\nstruct Bar;\n").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "fn baz() {}\n").unwrap();

        let output = list_definitions(tmp.path(), tmp.path()).unwrap();
        assert!(!output.is_error, "error: {}", output.content);

        let json: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        let defs = json["definitions"].as_array().unwrap();
        assert!(
            defs.len() >= 3,
            "expected at least 3 defs, got {}",
            defs.len()
        );
    }

    #[tokio::test]
    async fn list_definitions_skips_non_ast_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn foo() {}\n").unwrap();
        std::fs::write(
            tmp.path().join("README.md"),
            "# My Project\n\n## Getting Started\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("config.yaml"), "name: thing\nversion: 1\n").unwrap();

        let output = list_definitions(tmp.path(), tmp.path()).unwrap();
        assert!(!output.is_error, "error: {}", output.content);

        let json: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        let defs = json["definitions"].as_array().unwrap();
        // Only the .rs file contributes definitions.
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0]["name"], "foo");
    }
}
