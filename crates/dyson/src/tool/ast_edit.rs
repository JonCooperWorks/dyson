// ===========================================================================
// AstEdit tool — AST-aware code editing via tree-sitter.
//
// Unlike edit_file (string-based), this tool understands code structure.
// It parses files into concrete syntax trees and operates on identifier
// nodes, so renames don't accidentally modify strings, comments, or
// partial matches.
//
// Supported languages (11): Rust, Python, JavaScript, TypeScript, Go,
// JSON, C, C++, Java, Ruby, Bash.
//
// Operations:
//   - rename_symbol: Rename all occurrences of an identifier in a file.
//     Only touches AST identifier nodes — strings and comments are safe.
//   - list_definitions: Show top-level definitions (functions, structs,
//     classes, etc.) with line numbers.  Useful for planning before edits.
// ===========================================================================

use async_trait::async_trait;
use tree_sitter::{Node, Parser};

use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput, resolve_and_validate_path};

/// Maximum file size for AST parsing (10 MB).
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

pub struct AstEditTool;

#[async_trait]
impl Tool for AstEditTool {
    fn name(&self) -> &str {
        "ast_edit"
    }

    fn description(&self) -> &str {
        "AST-aware code editing via tree-sitter. Operations:\n\
         - rename_symbol: Rename identifiers without affecting strings/comments.\n\
         - list_definitions: Show top-level definitions with line numbers.\n\
         Supports: Rust, Python, JavaScript, TypeScript, Go, JSON, C, C++, Java, Ruby, Bash."
    }

    fn agent_only(&self) -> bool {
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file to edit or inspect"
                },
                "operation": {
                    "type": "string",
                    "enum": ["rename_symbol", "list_definitions"],
                    "description": "The AST operation to perform"
                },
                "old_name": {
                    "type": "string",
                    "description": "For rename_symbol: the current symbol name to rename"
                },
                "new_name": {
                    "type": "string",
                    "description": "For rename_symbol: the new symbol name"
                }
            },
            "required": ["file_path", "operation"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let file_path = input["file_path"]
            .as_str()
            .ok_or_else(|| DysonError::tool("ast_edit", "missing or invalid 'file_path'"))?;
        let operation = input["operation"]
            .as_str()
            .ok_or_else(|| DysonError::tool("ast_edit", "missing or invalid 'operation'"))?;

        let path = match resolve_and_validate_path(&ctx.working_dir, file_path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };

        // Check file exists and isn't too large.
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "cannot read '{}': {e}",
                    path.display()
                )));
            }
        };
        if metadata.len() > MAX_FILE_SIZE {
            return Ok(ToolOutput::error(format!(
                "file is too large ({} bytes, max {MAX_FILE_SIZE})",
                metadata.len()
            )));
        }

        let source = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "cannot read '{}': {e}",
                    path.display()
                )));
            }
        };

        // Detect language from file extension.
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        let lang = match language_for_extension(ext) {
            Some(l) => l,
            None => {
                return Ok(ToolOutput::error(format!(
                    "unsupported file type '.{ext}' — supported: .rs, .py, .js, .jsx, \
                     .ts, .tsx, .go, .json, .c, .h, .cpp, .cc, .java, .rb, .sh"
                )));
            }
        };

        // Parse with tree-sitter (CPU-bound, run on blocking thread).
        let source_clone = source.clone();
        let ext_owned = ext.to_string();
        let operation_owned = operation.to_string();
        let old_name = input["old_name"].as_str().map(|s| s.to_string());
        let new_name = input["new_name"].as_str().map(|s| s.to_string());
        let file_path_owned = file_path.to_string();
        let path_clone = path.clone();

        let result = tokio::task::spawn_blocking(move || {
            let mut parser = Parser::new();
            parser
                .set_language(&lang)
                .map_err(|e| DysonError::tool("ast_edit", format!("parser setup failed: {e}")))?;

            let tree = parser
                .parse(&source_clone, None)
                .ok_or_else(|| DysonError::tool("ast_edit", "parsing failed"))?;

            let root = tree.root_node();

            match operation_owned.as_str() {
                "rename_symbol" => {
                    let old = old_name.ok_or_else(|| {
                        DysonError::tool("ast_edit", "rename_symbol requires 'old_name'")
                    })?;
                    let new = new_name.ok_or_else(|| {
                        DysonError::tool("ast_edit", "rename_symbol requires 'new_name'")
                    })?;

                    if old.is_empty() || new.is_empty() {
                        return Ok(ToolOutput::error("old_name and new_name must not be empty"));
                    }
                    if old == new {
                        return Ok(ToolOutput::error(
                            "old_name and new_name are identical — nothing to do",
                        ));
                    }

                    do_rename_symbol(
                        root,
                        &source_clone,
                        &old,
                        &new,
                        &ext_owned,
                        &path_clone,
                        &file_path_owned,
                    )
                }
                "list_definitions" => do_list_definitions(root, &source_clone, &ext_owned, &file_path_owned),
                other => Ok(ToolOutput::error(format!(
                    "unknown operation '{other}' — use 'rename_symbol' or 'list_definitions'"
                ))),
            }
        })
        .await
        .map_err(|e| DysonError::tool("ast_edit", format!("task failed: {e}")))?;

        result
    }
}

// ---------------------------------------------------------------------------
// Language detection
// ---------------------------------------------------------------------------

/// Map file extension to a tree-sitter Language.
fn language_for_extension(ext: &str) -> Option<tree_sitter::Language> {
    match ext {
        "rs" => Some(tree_sitter_rust::LANGUAGE.into()),
        "py" => Some(tree_sitter_python::LANGUAGE.into()),
        "js" | "jsx" | "mjs" | "cjs" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "ts" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "tsx" => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        "json" => Some(tree_sitter_json::LANGUAGE.into()),
        "c" | "h" => Some(tree_sitter_c::LANGUAGE.into()),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => Some(tree_sitter_cpp::LANGUAGE.into()),
        "java" => Some(tree_sitter_java::LANGUAGE.into()),
        "rb" => Some(tree_sitter_ruby::LANGUAGE.into()),
        "sh" | "bash" => Some(tree_sitter_bash::LANGUAGE.into()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Identifier node types per language
// ---------------------------------------------------------------------------

/// Which AST node types represent "identifiers" for rename purposes.
///
/// These are the leaf nodes that carry symbol names.  By only matching
/// these types, we avoid touching string literals, comments, and other
/// non-identifier text that might coincidentally contain the same bytes.
fn identifier_node_types(ext: &str) -> &'static [&'static str] {
    match ext {
        "rs" => &["identifier", "type_identifier", "field_identifier"],
        "py" => &["identifier"],
        "js" | "jsx" | "mjs" | "cjs" => &[
            "identifier",
            "property_identifier",
            "shorthand_property_identifier",
        ],
        "ts" | "tsx" => &[
            "identifier",
            "property_identifier",
            "shorthand_property_identifier",
            "type_identifier",
        ],
        "go" => &["identifier", "type_identifier", "field_identifier"],
        "c" | "h" => &["identifier", "type_identifier", "field_identifier"],
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => &[
            "identifier",
            "type_identifier",
            "field_identifier",
            "namespace_identifier",
        ],
        "java" => &["identifier", "type_identifier"],
        "rb" => &["identifier", "constant"],
        "sh" | "bash" => &["variable_name", "command_name"],
        _ => &[],
    }
}

/// Which AST node types represent top-level definitions.
fn definition_node_types(ext: &str) -> &'static [&'static str] {
    match ext {
        "rs" => &[
            "function_item",
            "struct_item",
            "enum_item",
            "impl_item",
            "trait_item",
            "type_item",
            "const_item",
            "static_item",
            "mod_item",
            "macro_definition",
        ],
        "py" => &["function_definition", "class_definition"],
        "js" | "jsx" | "mjs" | "cjs" => &[
            "function_declaration",
            "class_declaration",
            "lexical_declaration",
        ],
        "ts" | "tsx" => &[
            "function_declaration",
            "class_declaration",
            "lexical_declaration",
            "interface_declaration",
            "type_alias_declaration",
        ],
        "go" => &[
            "function_declaration",
            "method_declaration",
            "type_declaration",
            "const_declaration",
            "var_declaration",
        ],
        "c" | "h" => &[
            "function_definition",
            "struct_specifier",
            "enum_specifier",
            "type_definition",
        ],
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => &[
            "function_definition",
            "class_specifier",
            "struct_specifier",
            "enum_specifier",
            "namespace_definition",
            "type_definition",
            "template_declaration",
        ],
        "java" => &[
            "class_declaration",
            "method_declaration",
            "interface_declaration",
            "enum_declaration",
            "constructor_declaration",
        ],
        "rb" => &["method", "singleton_method", "class", "module"],
        "sh" | "bash" => &["function_definition"],
        _ => &[],
    }
}

// ---------------------------------------------------------------------------
// rename_symbol operation
// ---------------------------------------------------------------------------

/// Rename all identifier occurrences of `old_name` to `new_name`.
fn do_rename_symbol(
    root: Node<'_>,
    source: &str,
    old_name: &str,
    new_name: &str,
    ext: &str,
    path: &std::path::Path,
    file_path_display: &str,
) -> Result<ToolOutput> {
    let id_types = identifier_node_types(ext);
    if id_types.is_empty() {
        return Ok(ToolOutput::error(format!(
            "rename_symbol is not supported for '.{ext}' files"
        )));
    }

    let source_bytes = source.as_bytes();
    let mut matches = Vec::new();
    collect_matching_identifiers(root, source_bytes, old_name, id_types, &mut matches);

    if matches.is_empty() {
        return Ok(ToolOutput::success(format!(
            "No identifier '{old_name}' found in {file_path_display}"
        )));
    }

    // Sort by start_byte descending so we can replace from end to start
    // without invalidating earlier byte offsets.
    matches.sort_by(|a, b| b.0.cmp(&a.0));

    let mut result = source.to_string();
    for (start, end) in &matches {
        result.replace_range(*start..*end, new_name);
    }

    std::fs::write(path, &result)
        .map_err(|e| DysonError::tool("ast_edit", format!("cannot write '{}': {e}", path.display())))?;

    let count = matches.len();
    Ok(ToolOutput::success(format!(
        "Renamed '{old_name}' -> '{new_name}': {count} occurrence(s) in {file_path_display}"
    )))
}

/// Recursively collect all identifier nodes matching `target_name`.
fn collect_matching_identifiers(
    node: Node<'_>,
    source: &[u8],
    target_name: &str,
    id_types: &[&str],
    results: &mut Vec<(usize, usize)>,
) {
    if id_types.contains(&node.kind()) {
        if let Ok(text) = std::str::from_utf8(&source[node.start_byte()..node.end_byte()]) {
            if text == target_name {
                results.push((node.start_byte(), node.end_byte()));
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_matching_identifiers(child, source, target_name, id_types, results);
    }
}

// ---------------------------------------------------------------------------
// list_definitions operation
// ---------------------------------------------------------------------------

/// List top-level definitions in the file.
fn do_list_definitions(
    root: Node<'_>,
    source: &str,
    ext: &str,
    file_path_display: &str,
) -> Result<ToolOutput> {
    let def_types = definition_node_types(ext);
    let source_bytes = source.as_bytes();

    let mut defs: Vec<(String, String, usize)> = Vec::new();

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        collect_definitions_recursive(child, source_bytes, def_types, ext, &mut defs, 0);
    }

    if defs.is_empty() {
        return Ok(ToolOutput::success(format!(
            "No definitions found in {file_path_display}"
        )));
    }

    let mut output = format!("## Definitions in {file_path_display}\n\n");
    for (kind, name, line) in &defs {
        // Clean up the kind for display (e.g., "function_item" -> "function").
        let display_kind = kind
            .replace("_item", "")
            .replace("_declaration", "")
            .replace("_definition", "");
        output.push_str(&format!("- {display_kind} **{name}** (line {line})\n"));
    }

    Ok(ToolOutput::success(output))
}

/// Collect definitions, recursing into impl/class blocks to find methods.
fn collect_definitions_recursive(
    node: Node<'_>,
    source: &[u8],
    def_types: &[&str],
    ext: &str,
    results: &mut Vec<(String, String, usize)>,
    depth: usize,
) {
    if depth > 2 {
        return; // Don't recurse too deep.
    }

    if def_types.contains(&node.kind()) {
        let name = extract_definition_name(&node, source)
            .unwrap_or_else(|| "<anonymous>".to_string());
        let line = node.start_position().row + 1;
        results.push((node.kind().to_string(), name, line));

        // For impl blocks, classes, etc., also list their children.
        if matches!(
            node.kind(),
            "impl_item"
                | "class_definition"
                | "class_declaration"
                | "class_specifier"
                | "namespace_definition"
                | "class"
                | "module"
        ) {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_definitions_recursive(child, source, def_types, ext, results, depth + 1);
            }
        }
    }
}

/// Extract the name from a definition node using tree-sitter field queries.
fn extract_definition_name(node: &Node<'_>, source: &[u8]) -> Option<String> {
    // Try the "name" field first — most languages use this.
    if let Some(name_node) = node.child_by_field_name("name") {
        let text = &source[name_node.start_byte()..name_node.end_byte()];
        return Some(String::from_utf8_lossy(text).to_string());
    }

    // For impl blocks in Rust, try the "type" field.
    if node.kind() == "impl_item" {
        if let Some(type_node) = node.child_by_field_name("type") {
            let text = &source[type_node.start_byte()..type_node.end_byte()];
            return Some(format!("impl {}", String::from_utf8_lossy(text)));
        }
    }

    // Fallback: first identifier-like child.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if kind == "identifier" || kind == "type_identifier" || kind == "property_identifier" {
            let text = &source[child.start_byte()..child.end_byte()];
            return Some(String::from_utf8_lossy(text).to_string());
        }
    }

    None
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    #[tokio::test]
    async fn rename_rust_symbol() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "fn old_func() -> i32 { 42 }\n\nfn main() {\n    let x = old_func();\n}\n",
        )
        .unwrap();

        let tool = AstEditTool;
        let input = serde_json::json!({
            "file_path": "lib.rs",
            "operation": "rename_symbol",
            "old_name": "old_func",
            "new_name": "new_func"
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(output.content.contains("2 occurrence(s)"));

        let content = std::fs::read_to_string(tmp.path().join("lib.rs")).unwrap();
        assert!(content.contains("new_func"));
        assert!(!content.contains("old_func"));
    }

    #[tokio::test]
    async fn rename_skips_strings_and_comments() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("test.rs"),
            "fn target() {}\n// target is a function\nlet s = \"target\";\n",
        )
        .unwrap();

        let tool = AstEditTool;
        let input = serde_json::json!({
            "file_path": "test.rs",
            "operation": "rename_symbol",
            "old_name": "target",
            "new_name": "renamed"
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error, "error: {}", output.content);

        let content = std::fs::read_to_string(tmp.path().join("test.rs")).unwrap();
        // The function name should be renamed.
        assert!(content.contains("fn renamed()"));
        // The comment should NOT be renamed.
        assert!(content.contains("// target is a function"));
        // The string should NOT be renamed.
        assert!(content.contains("\"target\""));
    }

    #[tokio::test]
    async fn rename_python_symbol() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("app.py"),
            "def old_func():\n    return 42\n\nresult = old_func()\n",
        )
        .unwrap();

        let tool = AstEditTool;
        let input = serde_json::json!({
            "file_path": "app.py",
            "operation": "rename_symbol",
            "old_name": "old_func",
            "new_name": "new_func"
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error, "error: {}", output.content);

        let content = std::fs::read_to_string(tmp.path().join("app.py")).unwrap();
        assert!(content.contains("def new_func():"));
        assert!(content.contains("new_func()"));
        assert!(!content.contains("old_func"));
    }

    #[tokio::test]
    async fn list_rust_definitions() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "struct Foo;\n\nfn bar() {}\n\nenum Baz { A, B }\n",
        )
        .unwrap();

        let tool = AstEditTool;
        let input = serde_json::json!({
            "file_path": "lib.rs",
            "operation": "list_definitions"
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(output.content.contains("Foo"));
        assert!(output.content.contains("bar"));
        assert!(output.content.contains("Baz"));
    }

    #[tokio::test]
    async fn unsupported_extension() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("data.csv"), "a,b,c\n1,2,3\n").unwrap();

        let tool = AstEditTool;
        let input = serde_json::json!({
            "file_path": "data.csv",
            "operation": "list_definitions"
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("unsupported"));
    }

    #[tokio::test]
    async fn rename_no_matches() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "fn hello() {}\n").unwrap();

        let tool = AstEditTool;
        let input = serde_json::json!({
            "file_path": "lib.rs",
            "operation": "rename_symbol",
            "old_name": "nonexistent",
            "new_name": "something"
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("No identifier"));
    }

    #[tokio::test]
    async fn rename_rejects_empty_names() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "fn hello() {}\n").unwrap();

        let tool = AstEditTool;
        let input = serde_json::json!({
            "file_path": "lib.rs",
            "operation": "rename_symbol",
            "old_name": "",
            "new_name": "something"
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("must not be empty"));
    }

    #[test]
    fn is_agent_only() {
        assert!(AstEditTool.agent_only());
    }

    #[test]
    fn language_detection() {
        // Original 6 + TSX
        assert!(language_for_extension("rs").is_some());
        assert!(language_for_extension("py").is_some());
        assert!(language_for_extension("js").is_some());
        assert!(language_for_extension("ts").is_some());
        assert!(language_for_extension("tsx").is_some());
        assert!(language_for_extension("go").is_some());
        assert!(language_for_extension("json").is_some());
        // New 5
        assert!(language_for_extension("c").is_some());
        assert!(language_for_extension("h").is_some());
        assert!(language_for_extension("cpp").is_some());
        assert!(language_for_extension("cc").is_some());
        assert!(language_for_extension("java").is_some());
        assert!(language_for_extension("rb").is_some());
        assert!(language_for_extension("sh").is_some());
        assert!(language_for_extension("bash").is_some());
        // Unsupported
        assert!(language_for_extension("csv").is_none());
        assert!(language_for_extension("").is_none());
    }
}
