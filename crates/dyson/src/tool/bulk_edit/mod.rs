// ===========================================================================
// BulkEdit tool — unified multi-file edit operations.
//
// Three operations dispatched via the `operation` field:
//   - rename_symbol: AST-aware identifier rename with text fallback for
//     non-grammar files (word-boundary matching).
//   - find_replace:  plain text or regex find-and-replace across files.
//   - list_definitions: AST-only listing of functions, classes, types, etc.
//
// Supported AST languages (19): Rust, Python, JavaScript, TypeScript, TSX,
// Go, Java, C, C++, C#, Ruby, Kotlin, Swift, Zig, Elixir, Erlang, OCaml,
// Haskell, Nix, JSON.  All grammars are statically linked.
// ===========================================================================

mod definitions;
mod find_replace;
mod rename;

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput, resolve_and_validate_path};

pub struct BulkEditTool;

#[async_trait]
impl Tool for BulkEditTool {
    fn name(&self) -> &str {
        "bulk_edit"
    }

    fn description(&self) -> &str {
        "Multi-file edit operations. Three modes: rename_symbol (AST-aware rename across files — \
         uses tree-sitter for supported languages, text fallback with word-boundary matching for \
         others), find_replace (plain text or regex find-and-replace across files), \
         list_definitions (lists functions, classes, types etc. using AST). Supported AST \
         languages: Rust, Python, JS/TS/TSX, Go, Java, C, C++, C#, Ruby, Kotlin, Swift, Zig, \
         Elixir, Erlang, OCaml, Haskell, Nix, JSON. \
         Mutating operations accept dry_run=true to preview without writing."
    }

    fn agent_only(&self) -> bool {
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["rename_symbol", "find_replace", "list_definitions"],
                    "description": "Which edit operation to perform"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory path. Use '.' for project root."
                },
                "old_name": {
                    "type": "string",
                    "description": "(rename_symbol) current symbol name"
                },
                "new_name": {
                    "type": "string",
                    "description": "(rename_symbol) new symbol name"
                },
                "pattern": {
                    "type": "string",
                    "description": "(find_replace) search string (or regex if regex=true)"
                },
                "replacement": {
                    "type": "string",
                    "description": "(find_replace) replacement string; supports $1/$2 capture groups when regex=true"
                },
                "regex": {
                    "type": "boolean",
                    "description": "(find_replace, optional, default false) treat pattern as a regex"
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "(rename_symbol, find_replace — optional, default false) preview changes without writing"
                }
            },
            "required": ["operation", "path"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let operation = input["operation"]
            .as_str()
            .ok_or_else(|| DysonError::tool("bulk_edit", "missing or invalid 'operation'"))?;
        let path_str = input["path"]
            .as_str()
            .ok_or_else(|| DysonError::tool("bulk_edit", "missing or invalid 'path'"))?;

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
                        return Ok(ToolOutput::error("rename_symbol requires 'old_name'"));
                    }
                };
                let new_name = match input["new_name"].as_str() {
                    Some(s) if !s.is_empty() => s.to_string(),
                    Some(_) => {
                        return Ok(ToolOutput::error("new_name must not be empty"));
                    }
                    None => {
                        return Ok(ToolOutput::error("rename_symbol requires 'new_name'"));
                    }
                };
                if old_name == new_name {
                    return Ok(ToolOutput::error(
                        "old_name and new_name are identical — nothing to do",
                    ));
                }
                let dry_run = input["dry_run"].as_bool().unwrap_or(false);

                tokio::task::spawn_blocking(move || {
                    rename::rename_symbol(&resolved, &working_dir, &old_name, &new_name, dry_run)
                })
                .await
                .map_err(|e| DysonError::tool("bulk_edit", format!("task failed: {e}")))?
            }
            "find_replace" => {
                let pattern = match input["pattern"].as_str() {
                    Some(s) => s.to_string(),
                    None => {
                        return Ok(ToolOutput::error("find_replace requires 'pattern'"));
                    }
                };
                let replacement = match input["replacement"].as_str() {
                    Some(s) => s.to_string(),
                    None => {
                        return Ok(ToolOutput::error("find_replace requires 'replacement'"));
                    }
                };
                let use_regex = input["regex"].as_bool().unwrap_or(false);
                let dry_run = input["dry_run"].as_bool().unwrap_or(false);

                if let Err(msg) = find_replace::validate(&pattern, &replacement, use_regex) {
                    return Ok(ToolOutput::error(msg));
                }

                tokio::task::spawn_blocking(move || {
                    find_replace::find_replace(
                        &resolved,
                        &working_dir,
                        &pattern,
                        &replacement,
                        use_regex,
                        dry_run,
                    )
                })
                .await
                .map_err(|e| DysonError::tool("bulk_edit", format!("task failed: {e}")))?
            }
            "list_definitions" => tokio::task::spawn_blocking(move || {
                definitions::list_definitions(&resolved, &working_dir)
            })
            .await
            .map_err(|e| DysonError::tool("bulk_edit", format!("task failed: {e}")))?,
            other => Ok(ToolOutput::error(format!(
                "unknown operation '{other}' — use 'rename_symbol', 'find_replace', or 'list_definitions'"
            ))),
        }
    }
}

// ===========================================================================
// Tests — dispatch + validation. Operation-specific tests live in the
// submodules (rename.rs, find_replace.rs, definitions.rs).
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    #[tokio::test]
    async fn unknown_operation_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = BulkEditTool;
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
        assert!(output.content.contains("find_replace"));
    }

    #[tokio::test]
    async fn rename_identical_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = BulkEditTool;
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
        let tool = BulkEditTool;

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
    async fn find_replace_rejects_empty_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = BulkEditTool;
        let input = serde_json::json!({
            "path": ".",
            "operation": "find_replace",
            "pattern": "",
            "replacement": "something"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("must not be empty"));
    }

    #[tokio::test]
    async fn find_replace_rejects_identical_strings() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = BulkEditTool;
        let input = serde_json::json!({
            "path": ".",
            "operation": "find_replace",
            "pattern": "same",
            "replacement": "same"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("identical"));
    }

    #[tokio::test]
    async fn dispatch_to_list_definitions() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "fn hello() {}\n").unwrap();

        let tool = BulkEditTool;
        let input = serde_json::json!({
            "path": "lib.rs",
            "operation": "list_definitions"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        let json: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        let defs = json["definitions"].as_array().unwrap();
        assert!(!defs.is_empty());
    }

    #[test]
    fn is_agent_only() {
        assert!(BulkEditTool.agent_only());
    }
}
