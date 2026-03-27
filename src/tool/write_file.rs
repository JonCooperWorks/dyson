// ===========================================================================
// WriteFile tool — create or overwrite a file with given content.
// ===========================================================================

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput, resolve_and_validate_path};

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Create or overwrite a file with the given content. \
         Parent directories are created automatically if they don't exist."
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
                    "description": "Path to the file (relative to working directory or absolute)"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                }
            },
            "required": ["file_path", "content"]
        })
    }

    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let file_path = input["file_path"]
            .as_str()
            .ok_or_else(|| DysonError::tool("write_file", "missing or invalid 'file_path'"))?;
        let content = input["content"]
            .as_str()
            .ok_or_else(|| DysonError::tool("write_file", "missing or invalid 'content'"))?;

        let path = match resolve_and_validate_path(&ctx.working_dir, file_path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };

        // Create parent directories if needed.
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return Ok(ToolOutput::error(format!(
                        "cannot create directories for '{}': {e}",
                        path.display()
                    )));
                }
            }
        }

        let bytes = content.len();
        if let Err(e) = tokio::fs::write(&path, content).await {
            return Ok(ToolOutput::error(format!(
                "cannot write '{}': {e}",
                path.display()
            )));
        }

        Ok(ToolOutput::success(format!(
            "Wrote {bytes} bytes to {}",
            file_path
        )))
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
    async fn write_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = WriteFileTool;
        let input = serde_json::json!({
            "file_path": "hello.txt",
            "content": "hello world"
        });
        let output = tool.run(input, &test_ctx(tmp.path())).await.unwrap();
        assert!(!output.is_error);

        let content = std::fs::read_to_string(tmp.path().join("hello.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = WriteFileTool;
        let input = serde_json::json!({
            "file_path": "a/b/c.txt",
            "content": "nested"
        });
        let output = tool.run(input, &test_ctx(tmp.path())).await.unwrap();
        assert!(!output.is_error);

        let content = std::fs::read_to_string(tmp.path().join("a/b/c.txt")).unwrap();
        assert_eq!(content, "nested");
    }

    #[tokio::test]
    async fn overwrite_existing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("existing.txt"), "old").unwrap();

        let tool = WriteFileTool;
        let input = serde_json::json!({
            "file_path": "existing.txt",
            "content": "new"
        });
        let output = tool.run(input, &test_ctx(tmp.path())).await.unwrap();
        assert!(!output.is_error);

        let content = std::fs::read_to_string(tmp.path().join("existing.txt")).unwrap();
        assert_eq!(content, "new");
    }

    #[test]
    fn is_agent_only() {
        assert!(WriteFileTool.agent_only());
    }
}
