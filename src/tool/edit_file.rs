// ===========================================================================
// EditFile tool — find-and-replace editing within a file.
// ===========================================================================

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput, resolve_and_validate_path};

/// Maximum file size we'll load into memory for editing (10 MB).
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing an exact string with a new string. \
         The old_string must appear exactly once in the file for safety. \
         Use this for surgical edits — for full rewrites use write_file instead."
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
                    "description": "Path to the file to edit"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact string to find and replace (must appear exactly once)"
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement string"
                }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let file_path = input["file_path"]
            .as_str()
            .ok_or_else(|| DysonError::tool("edit_file", "missing or invalid 'file_path'"))?;
        let old_string = input["old_string"]
            .as_str()
            .ok_or_else(|| DysonError::tool("edit_file", "missing or invalid 'old_string'"))?;
        let new_string = input["new_string"]
            .as_str()
            .ok_or_else(|| DysonError::tool("edit_file", "missing or invalid 'new_string'"))?;

        if old_string.is_empty() {
            return Ok(ToolOutput::error("old_string must not be empty"));
        }

        let path = match resolve_and_validate_path(&ctx.working_dir, file_path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };

        // Check file size before reading.
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

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "cannot read '{}': {e}",
                    path.display()
                )));
            }
        };

        // Count occurrences to ensure exactly one match.
        let count = content.matches(old_string).count();
        if count == 0 {
            return Ok(ToolOutput::error(
                "old_string not found in file — no changes made",
            ));
        }
        if count > 1 {
            return Ok(ToolOutput::error(format!(
                "old_string appears {count} times — must appear exactly once for safe editing. \
                 Provide more surrounding context to make the match unique."
            )));
        }

        let new_content = content.replacen(old_string, new_string, 1);

        if let Err(e) = tokio::fs::write(&path, &new_content).await {
            return Ok(ToolOutput::error(format!(
                "cannot write '{}': {e}",
                path.display()
            )));
        }

        Ok(ToolOutput::success(format!("Applied edit to {file_path}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    fn test_ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            env: std::collections::HashMap::new(),
            cancellation: tokio_util::sync::CancellationToken::new(),
            workspace: None,
            depth: 0,
        }
    }

    #[tokio::test]
    async fn edit_replaces_string() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("test.rs"),
            "fn hello() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();

        let tool = EditFileTool;
        let input = serde_json::json!({
            "file_path": "test.rs",
            "old_string": "println!(\"hello\")",
            "new_string": "println!(\"world\")"
        });
        let output = tool.run(input, &test_ctx(tmp.path())).await.unwrap();
        assert!(!output.is_error);

        let content = std::fs::read_to_string(tmp.path().join("test.rs")).unwrap();
        assert!(content.contains("println!(\"world\")"));
        assert!(!content.contains("println!(\"hello\")"));
    }

    #[tokio::test]
    async fn edit_rejects_multiple_matches() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "aaa bbb aaa").unwrap();

        let tool = EditFileTool;
        let input = serde_json::json!({
            "file_path": "test.txt",
            "old_string": "aaa",
            "new_string": "ccc"
        });
        let output = tool.run(input, &test_ctx(tmp.path())).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("2 times"));
    }

    #[tokio::test]
    async fn edit_rejects_no_match() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "hello world").unwrap();

        let tool = EditFileTool;
        let input = serde_json::json!({
            "file_path": "test.txt",
            "old_string": "xyz",
            "new_string": "abc"
        });
        let output = tool.run(input, &test_ctx(tmp.path())).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("not found"));
    }

    #[test]
    fn is_agent_only() {
        assert!(EditFileTool.agent_only());
    }
}
