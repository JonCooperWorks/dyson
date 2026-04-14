// ===========================================================================
// AstEdit tool — AST-aware code operations via tree-sitter.
//
// Operations:
//   - rename_symbol: Rename identifiers across files, skipping strings
//     and comments.  Supports single files and directories.
//   - list_definitions: List functions, classes, types, etc. with line
//     numbers.  Supports single files and directories.
//
// Supported languages (19): Rust, Python, JavaScript, TypeScript, TSX,
// Go, Java, C, C++, C#, Ruby, Kotlin, Swift, Zig, Elixir, Erlang,
// OCaml, Haskell, Nix, JSON (list_definitions only).
//
// All 20 tree-sitter grammars are statically linked — no dynamic loading
// or network calls.
// ===========================================================================

mod definitions;
mod languages;
mod rename;

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput, resolve_and_validate_path};

pub struct AstEditTool;

#[async_trait]
impl Tool for AstEditTool {
    fn name(&self) -> &str {
        "ast_edit"
    }

    fn description(&self) -> &str {
        "AST-aware code operations using tree-sitter. \
         rename_symbol renames identifiers across files (skipping strings/comments). \
         list_definitions lists functions, classes, types, etc. with line numbers. \
         Supports: Rust, Python, JS/TS/TSX, Go, Java, C, C++, C#, Ruby, Kotlin, \
         Swift, Zig, Elixir, Erlang, OCaml, Haskell, Nix, JSON."
    }

    fn agent_only(&self) -> bool {
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File or directory path. Use '.' for project root."
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
            "required": ["path", "operation"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let path_str = input["path"]
            .as_str()
            .ok_or_else(|| DysonError::tool("ast_edit", "missing or invalid 'path'"))?;
        let operation = input["operation"]
            .as_str()
            .ok_or_else(|| DysonError::tool("ast_edit", "missing or invalid 'operation'"))?;

        let resolved = match resolve_and_validate_path(&ctx.working_dir, path_str) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };

        let working_dir = ctx.working_dir.clone();

        match operation {
            "rename_symbol" => {
                let old_name = match input["old_name"].as_str() {
                    Some(s) if !s.is_empty() => s.to_string(),
                    Some(_) => {
                        return Ok(ToolOutput::error("old_name must not be empty"));
                    }
                    None => {
                        return Ok(ToolOutput::error(
                            "rename_symbol requires 'old_name'",
                        ));
                    }
                };
                let new_name = match input["new_name"].as_str() {
                    Some(s) if !s.is_empty() => s.to_string(),
                    Some(_) => {
                        return Ok(ToolOutput::error("new_name must not be empty"));
                    }
                    None => {
                        return Ok(ToolOutput::error(
                            "rename_symbol requires 'new_name'",
                        ));
                    }
                };

                if old_name == new_name {
                    return Ok(ToolOutput::error(
                        "old_name and new_name are identical — nothing to do",
                    ));
                }

                tokio::task::spawn_blocking(move || {
                    rename::rename_symbol(&resolved, &working_dir, &old_name, &new_name)
                })
                .await
                .map_err(|e| DysonError::tool("ast_edit", format!("task failed: {e}")))?
            }
            "list_definitions" => {
                tokio::task::spawn_blocking(move || {
                    definitions::list_definitions(&resolved, &working_dir)
                })
                .await
                .map_err(|e| DysonError::tool("ast_edit", format!("task failed: {e}")))?
            }
            other => Ok(ToolOutput::error(format!(
                "unknown operation '{other}' — use 'rename_symbol' or 'list_definitions'"
            ))),
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    #[tokio::test]
    async fn rename_rust_basics() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "fn target() -> i32 { 42 }\n\n\
             fn main() {\n    let x = target();\n}\n\
             // target is important\n\
             let s = \"target\";\n",
        )
        .unwrap();

        let tool = AstEditTool;
        let input = serde_json::json!({
            "path": "lib.rs",
            "operation": "rename_symbol",
            "old_name": "target",
            "new_name": "renamed"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);

        let json: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(json["occurrences_renamed"], 2);
        assert_eq!(json["files_modified"], 1);

        let content = std::fs::read_to_string(tmp.path().join("lib.rs")).unwrap();
        assert!(content.contains("fn renamed()"));
        assert!(content.contains("renamed();\n"));
        // Comments and strings should NOT be renamed.
        assert!(content.contains("// target is important"));
        assert!(content.contains("\"target\""));
    }

    #[tokio::test]
    async fn rename_across_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/a.rs"),
            "struct Config { val: i32 }\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("src/b.rs"),
            "fn new_config() -> Config { Config { val: 1 } }\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("src/c.rs"),
            "use crate::Config;\nfn get() -> Config { todo!() }\n",
        )
        .unwrap();

        let tool = AstEditTool;
        let input = serde_json::json!({
            "path": "src",
            "operation": "rename_symbol",
            "old_name": "Config",
            "new_name": "AppConfig"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);

        let json: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(json["files_modified"], 3);
        assert!(json["occurrences_renamed"].as_u64().unwrap() >= 5);

        // Verify all files were actually modified.
        for name in &["src/a.rs", "src/b.rs", "src/c.rs"] {
            let content = std::fs::read_to_string(tmp.path().join(name)).unwrap();
            assert!(
                content.contains("AppConfig"),
                "{name} should contain AppConfig"
            );
            assert!(
                !content.contains("Config") || content.contains("AppConfig"),
                "{name} should not contain bare Config"
            );
        }
    }

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

        let tool = AstEditTool;
        let input = serde_json::json!({
            "path": "app.py",
            "operation": "list_definitions"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);

        let json: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        let defs = json["definitions"].as_array().unwrap();
        assert!(defs.len() >= 3, "expected at least 3 definitions, got {}", defs.len());

        let names: Vec<&str> = defs
            .iter()
            .filter_map(|d| d["name"].as_str())
            .collect();
        assert!(names.contains(&"hello"));
        assert!(names.contains(&"MyClass"));
        assert!(names.contains(&"goodbye"));
    }

    #[tokio::test]
    async fn rename_no_matches() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "fn hello() {}\n").unwrap();

        let tool = AstEditTool;
        let input = serde_json::json!({
            "path": "lib.rs",
            "operation": "rename_symbol",
            "old_name": "nonexistent",
            "new_name": "something"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error);

        let json: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(json["files_modified"], 0);
        assert_eq!(json["occurrences_renamed"], 0);
    }

    #[tokio::test]
    async fn rename_substring_no_match() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "struct Config {}\nstruct ConfigManager {}\n",
        )
        .unwrap();

        let tool = AstEditTool;
        let input = serde_json::json!({
            "path": "lib.rs",
            "operation": "rename_symbol",
            "old_name": "Config",
            "new_name": "AppConfig"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);

        let content = std::fs::read_to_string(tmp.path().join("lib.rs")).unwrap();
        assert!(content.contains("AppConfig"));
        assert!(content.contains("ConfigManager"));
        // ConfigManager should NOT become AppConfigManager.
        assert!(!content.contains("AppConfigManager"));
    }

    #[tokio::test]
    async fn rename_binary_file_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        // Binary file with .rs extension.
        std::fs::write(tmp.path().join("binary.rs"), &[0u8, 159, 146, 150]).unwrap();
        std::fs::write(tmp.path().join("good.rs"), "fn target() {}\n").unwrap();

        let tool = AstEditTool;
        let input = serde_json::json!({
            "path": ".",
            "operation": "rename_symbol",
            "old_name": "target",
            "new_name": "renamed"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);

        let json: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(json["files_modified"], 1);
    }

    #[tokio::test]
    async fn rename_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("empty")).unwrap();

        let tool = AstEditTool;
        let input = serde_json::json!({
            "path": "empty",
            "operation": "rename_symbol",
            "old_name": "foo",
            "new_name": "bar"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error);

        let json: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(json["files_modified"], 0);
        assert_eq!(json["occurrences_renamed"], 0);
    }

    #[tokio::test]
    async fn rename_identical_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = AstEditTool;
        let input = serde_json::json!({
            "path": ".",
            "operation": "rename_symbol",
            "old_name": "same",
            "new_name": "same"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("identical"));
    }

    #[tokio::test]
    async fn rename_empty_names_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = AstEditTool;

        let output = tool
            .run(
                &serde_json::json!({
                    "path": ".",
                    "operation": "rename_symbol",
                    "old_name": "",
                    "new_name": "something"
                }),
                &ToolContext::for_test(tmp.path()),
            )
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("must not be empty"));

        let output = tool
            .run(
                &serde_json::json!({
                    "path": ".",
                    "operation": "rename_symbol",
                    "old_name": "something",
                    "new_name": ""
                }),
                &ToolContext::for_test(tmp.path()),
            )
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("must not be empty"));
    }

    #[tokio::test]
    async fn unsupported_extension_silently_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("data.csv"), "target,value\n1,2\n").unwrap();
        std::fs::write(tmp.path().join("good.rs"), "fn target() {}\n").unwrap();

        let tool = AstEditTool;
        let input = serde_json::json!({
            "path": ".",
            "operation": "rename_symbol",
            "old_name": "target",
            "new_name": "renamed"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error);

        let json: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        // Only the .rs file should be modified, .csv silently skipped.
        assert_eq!(json["files_modified"], 1);

        // CSV should be untouched.
        let csv = std::fs::read_to_string(tmp.path().join("data.csv")).unwrap();
        assert!(csv.contains("target"));
    }

    #[tokio::test]
    async fn list_definitions_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn foo() {}\nstruct Bar;\n").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "fn baz() {}\n").unwrap();

        let tool = AstEditTool;
        let input = serde_json::json!({
            "path": ".",
            "operation": "list_definitions"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);

        let json: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        let defs = json["definitions"].as_array().unwrap();
        assert!(defs.len() >= 3, "expected at least 3 defs, got {}", defs.len());
    }

    #[tokio::test]
    async fn unknown_operation_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = AstEditTool;
        let input = serde_json::json!({
            "path": ".",
            "operation": "invalid_op"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("unknown operation"));
    }

    #[test]
    fn is_agent_only() {
        assert!(AstEditTool.agent_only());
    }
}
