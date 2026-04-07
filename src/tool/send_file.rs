// ===========================================================================
// SendFile tool — send a file to the user via the controller.
//
// This tool is controller-agnostic: it attaches the file path to the
// ToolOutput, and the controller's Output::send_file() implementation
// handles delivery (e.g. sending a document message, printing the path).
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
         current controller (e.g., as a document message, a file path, \
         etc.). Use this when the user asks you to send, share, or \
         deliver a file."
    }

    fn agent_only(&self) -> bool {
        // NOT agent_only: this delivers files to the user through the
        // controller (Telegram document, terminal path, etc.), not a
        // filesystem operation that CLI tools duplicate.
        false
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

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let file_path = input["file_path"]
            .as_str()
            .ok_or_else(|| DysonError::tool("send_file", "missing or invalid 'file_path'"))?;

        let path = if ctx.dangerous_no_sandbox {
            // No sandbox — resolve the path without boundary checks.
            let candidate = if std::path::Path::new(file_path).is_absolute() {
                std::path::PathBuf::from(file_path)
            } else {
                ctx.working_dir.join(file_path)
            };
            match candidate.canonicalize() {
                Ok(p) => p,
                Err(e) => return Ok(ToolOutput::error(format!(
                    "cannot resolve path '{}': {e}", candidate.display()
                ))),
            }
        } else {
            match resolve_and_validate_path(&ctx.working_dir, file_path) {
                Ok(p) => p,
                Err(e) => return Ok(ToolOutput::error(e)),
            }
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

    #[tokio::test]
    async fn send_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("report.pdf");
        let mut f = std::fs::File::create(&file).unwrap();
        writeln!(f, "fake pdf content").unwrap();

        let tool = SendFileTool;
        let input = serde_json::json!({"file_path": "report.pdf"});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert_eq!(output.files.len(), 1);
        assert_eq!(output.files[0], file.canonicalize().unwrap());
    }

    #[tokio::test]
    async fn send_nonexistent_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SendFileTool;
        let input = serde_json::json!({"file_path": "nope.txt"});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(output.is_error);
        assert!(output.files.is_empty());
    }

    #[tokio::test]
    async fn send_directory_fails() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();

        let tool = SendFileTool;
        let input = serde_json::json!({"file_path": "subdir"});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("not a file"));
    }

    #[test]
    fn is_not_agent_only() {
        // send_file delivers to the user via the controller — not a
        // filesystem op, so it should be available to all backends.
        assert!(!SendFileTool.agent_only());
    }
}
