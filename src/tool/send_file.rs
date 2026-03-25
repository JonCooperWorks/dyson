// ===========================================================================
// SendFile tool — send a file to the user via the controller.
//
// Unlike read_file (which returns file *contents* to the LLM), send_file
// delivers the actual file to the user through the controller's side-channel:
//   - Terminal: prints the file path
//   - Telegram: sends the file as a document via sendDocument
//
// This is useful when the user asks for a file (PDF, image, archive, etc.)
// and the LLM needs to deliver it rather than paste its contents.
// ===========================================================================

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::tool::{resolve_and_validate_path, Tool, ToolContext, ToolOutput};

pub struct SendFileTool;

#[async_trait]
impl Tool for SendFileTool {
    fn name(&self) -> &str {
        "send_file"
    }

    fn description(&self) -> &str {
        "Send a file to the user. Use this when the user asks for a file \
         (e.g., \"send me the report\", \"give me that PDF\", \"download \
         the log file\"). The file is delivered directly to the user — \
         on Telegram it arrives as a document, on the terminal the path \
         is printed. Do NOT use read_file for this purpose; read_file \
         returns file contents to you (the LLM), while send_file delivers \
         the file to the user."
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
                "file not found: '{}'",
                path.display()
            )));
        }

        if !path.is_file() {
            return Ok(ToolOutput::error(format!(
                "not a file: '{}'",
                path.display()
            )));
        }

        // Get file metadata for the confirmation message.
        let metadata = tokio::fs::metadata(&path).await.map_err(|e| {
            DysonError::tool("send_file", format!("cannot stat '{}': {e}", path.display()))
        })?;

        let size_bytes = metadata.len();
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| file_path.to_string());

        Ok(ToolOutput::success(format!(
            "Sent file '{file_name}' ({} bytes) to the user.",
            size_bytes
        ))
        .with_file(&path))
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
        }
    }

    #[tokio::test]
    async fn sends_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("report.pdf");
        let mut f = std::fs::File::create(&file).unwrap();
        f.write_all(b"fake pdf content").unwrap();

        let tool = SendFileTool;
        let input = serde_json::json!({"file_path": "report.pdf"});
        let output = tool.run(input, &test_ctx(tmp.path())).await.unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("report.pdf"));
        assert!(output.content.contains("16 bytes"));
        assert_eq!(output.files.len(), 1);
        assert_eq!(output.files[0], file.canonicalize().unwrap());
    }

    #[tokio::test]
    async fn error_on_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SendFileTool;
        let input = serde_json::json!({"file_path": "nope.txt"});
        let output = tool.run(input, &test_ctx(tmp.path())).await.unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("file not found"));
        assert!(output.files.is_empty());
    }

    #[tokio::test]
    async fn error_on_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("subdir");
        std::fs::create_dir(&dir).unwrap();

        let tool = SendFileTool;
        let input = serde_json::json!({"file_path": "subdir"});
        let output = tool.run(input, &test_ctx(tmp.path())).await.unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("not a file"));
        assert!(output.files.is_empty());
    }

    #[tokio::test]
    async fn error_on_missing_param() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SendFileTool;
        let input = serde_json::json!({});
        let result = tool.run(input, &test_ctx(tmp.path())).await;

        assert!(result.is_err());
    }
}
