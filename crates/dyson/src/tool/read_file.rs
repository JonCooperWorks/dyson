// ===========================================================================
// ReadFile tool — read file contents with optional line range.
// ===========================================================================

use std::fmt::Write as _;

use async_trait::async_trait;
use tokio::io::AsyncBufReadExt;

use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput, resolve_and_validate_path};
use crate::util::truncate_output;

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Returns lines with line numbers. \
         Use `offset` (1-based line number) and `limit` (number of lines) \
         to read a specific range. PDF files are automatically detected \
         and their text content is extracted."
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
                "offset": {
                    "type": "integer",
                    "description": "1-based line number to start reading from (default: 1)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read (default: all)"
                }
            },
            "required": ["file_path"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let file_path = input["file_path"]
            .as_str()
            .ok_or_else(|| DysonError::tool("read_file", "missing or invalid 'file_path'"))?;

        let path = match resolve_and_validate_path(&ctx.working_dir, file_path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e)),
        };

        let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = input["limit"].as_u64().map(|l| l as usize);

        // Guard against reading very large files into memory.
        const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB
        match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.len() > MAX_FILE_SIZE => {
                return Ok(ToolOutput::error(format!(
                    "file '{}' is too large ({:.1} MB, limit is {:.0} MB)",
                    path.display(),
                    meta.len() as f64 / (1024.0 * 1024.0),
                    MAX_FILE_SIZE as f64 / (1024.0 * 1024.0),
                )));
            }
            Err(e) => {
                return Ok(ToolOutput::error(super::path_err("stat", &path, e)));
            }
            _ => {}
        }

        // PDF files: extract text instead of reading raw binary.
        if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("pdf")) {
            let data = match tokio::fs::read(&path).await {
                Ok(d) => d,
                Err(e) => {
                    return Ok(ToolOutput::error(super::path_err("read", &path, e)));
                }
            };

            let text = match pdf_extract::extract_text_from_mem(&data) {
                Ok(t) => t,
                Err(e) => {
                    return Ok(ToolOutput::error(format!(
                        "failed to extract text from '{}': {e}",
                        path.display()
                    )));
                }
            };

            if text.trim().is_empty() {
                return Ok(ToolOutput::success(
                    "(PDF contains no extractable text — may be scanned/image-only)",
                ));
            }

            let output = truncate_output(&text);
            return Ok(ToolOutput::success(output));
        }

        // Stream line-by-line with skip/take so that large files with small
        // offset/limit ranges don't need to be read entirely into memory.
        let file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "cannot read '{}': {e}",
                    path.display()
                )));
            }
        };

        let reader = tokio::io::BufReader::new(file);
        let mut lines = reader.lines();
        let start = offset - 1;

        // Skip lines before the requested offset.
        for _ in 0..start {
            match lines.next_line().await {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => {
                    return Ok(ToolOutput::error(super::path_err("read", &path, e)));
                }
            }
        }

        // Read the requested range.
        let max_lines = limit.unwrap_or(usize::MAX);
        let mut output = String::new();
        let mut count = 0;
        while count < max_lines {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let line_num = start + count + 1;
                    let _ = writeln!(output, "{line_num:>6}\t{line}");
                    count += 1;
                }
                Ok(None) => break,
                Err(e) => {
                    return Ok(ToolOutput::error(super::path_err("read", &path, e)));
                }
            }
        }

        // Truncate if too large.
        let output = truncate_output(&output);

        if output.is_empty() {
            return Ok(ToolOutput::success("(empty file)"));
        }

        Ok(ToolOutput::success(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use std::io::Write;

    #[tokio::test]
    async fn read_simple_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("test.txt");
        let mut f = std::fs::File::create(&file).unwrap();
        writeln!(f, "line one").unwrap();
        writeln!(f, "line two").unwrap();
        writeln!(f, "line three").unwrap();

        let tool = ReadFileTool;
        let input = serde_json::json!({"file_path": "test.txt"});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("line one"));
        assert!(output.content.contains("line three"));
    }

    #[tokio::test]
    async fn read_with_offset_and_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("test.txt");
        let mut f = std::fs::File::create(&file).unwrap();
        for i in 1..=10 {
            writeln!(f, "line {i}").unwrap();
        }

        let tool = ReadFileTool;
        let input = serde_json::json!({"file_path": "test.txt", "offset": 3, "limit": 2});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("line 3"));
        assert!(output.content.contains("line 4"));
        assert!(!output.content.contains("line 5"));
    }

    #[tokio::test]
    async fn read_nonexistent_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ReadFileTool;
        let input = serde_json::json!({"file_path": "nope.txt"});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(output.is_error);
    }

    #[test]
    fn is_agent_only() {
        assert!(ReadFileTool.agent_only());
    }
}
