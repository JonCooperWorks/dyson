// ===========================================================================
// SendFile tool — send a file to the user via the controller.
//
// This tool is controller-agnostic: it attaches the file path to the
// ToolOutput, and the controller's Output::send_file() implementation
// handles delivery (Telegram sends a document, terminal prints the path,
// etc.).
// ===========================================================================

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput, resolve_and_validate_path};

pub struct SendFileTool;

#[async_trait]
impl Tool for SendFileTool {
    fn name(&self) -> &str {
        "send_file"
    }

    fn description(&self) -> &str {
        "Send a file to the user. The file will be delivered through the \
         current controller (e.g., as a Telegram document, a terminal file \
         path, etc.). Use this when the user asks you to send, share, or \
         deliver a file."
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
                    "description": "Path to the file to send (relative to working directory or absolute)"
                }
            },
            "required": ["file_path"]
        })
    }

    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let file_path = input["file_path"]
            .as_str()
            .ok_or_else(|| DysonError::tool("send_file", "missing or invalid 'file_path'"))?;

        let path = match resolve_and_validate_path(&ctx.working_dir, file_path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };

        if !path.exists() {
            return Ok(ToolOutput::error(format!(
                "file does not exist: '{}'",
                path.display()
            )));
        }

        if !path.is_file() {
            return Ok(ToolOutput::error(format!(
                "path is not a file: '{}'",
                path.display()
            )));
        }

        Ok(ToolOutput::success(format!("Sent file: {}", path.display())).with_file(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use std::io::Write;

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
    async fn send_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("report.pdf");
        let mut f = std::fs::File::create(&file).unwrap();
        writeln!(f, "fake pdf content").unwrap();

        let tool = SendFileTool;
        let input = serde_json::json!({"file_path": "report.pdf"});
        let output = tool.run(input, &test_ctx(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert_eq!(output.files.len(), 1);
        assert_eq!(output.files[0], file.canonicalize().unwrap());
    }

    #[tokio::test]
    async fn send_nonexistent_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SendFileTool;
        let input = serde_json::json!({"file_path": "nope.txt"});
        let output = tool.run(input, &test_ctx(tmp.path())).await.unwrap();
        assert!(output.is_error);
        assert!(output.files.is_empty());
    }

    #[tokio::test]
    async fn send_directory_fails() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();

        let tool = SendFileTool;
        let input = serde_json::json!({"file_path": "subdir"});
        let output = tool.run(input, &test_ctx(tmp.path())).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("not a file"));
    }

    #[test]
    fn is_agent_only() {
        assert!(SendFileTool.agent_only());
    }
}
