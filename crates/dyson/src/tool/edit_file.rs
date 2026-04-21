// ===========================================================================
// EditFile tool — find-and-replace editing within a file.
// ===========================================================================

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput, path_err};

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

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
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

        let path = match ctx.resolve_path(file_path) { Ok(p) => p, Err(e) => return Ok(e) };

        // Check file size before reading.
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolOutput::error(path_err("read", &path, e)));
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
                return Ok(ToolOutput::error(path_err("read", &path, e)));
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

        // Find the line number of the first changed line so the diff
        // renders with real line numbers from the source file.
        let match_byte = content.find(old_string).unwrap_or(0);
        let start_line = content[..match_byte].chars().filter(|c| *c == '\n').count() + 1;

        if let Err(e) = tokio::fs::write(&path, &new_content).await {
            return Ok(ToolOutput::error(path_err("write", &path, e)));
        }

        let view = build_diff_view(file_path, start_line, old_string, new_string);
        Ok(ToolOutput::success(format!("Applied edit to {file_path}")).with_view(view))
    }
}

/// Build a minimal `Diff` view: removed lines from `old_string`, added
/// lines from `new_string`, no surrounding context.  Sufficient for the
/// right-rail panel to render the change clearly.
fn build_diff_view(
    file_path: &str,
    start_line: usize,
    old_string: &str,
    new_string: &str,
) -> crate::tool::view::ToolView {
    use crate::tool::view::{DiffFile, DiffRow, ToolView};
    let old_lines: Vec<&str> = old_string.lines().collect();
    let new_lines: Vec<&str> = new_string.lines().collect();
    let mut rows: Vec<DiffRow> = Vec::with_capacity(old_lines.len() + new_lines.len());
    for (i, l) in old_lines.iter().enumerate() {
        rows.push(DiffRow {
            t: "rem".into(),
            ln: start_line + i,
            sn: "-".into(),
            l: (*l).to_string(),
        });
    }
    for (i, l) in new_lines.iter().enumerate() {
        rows.push(DiffRow {
            t: "add".into(),
            ln: start_line + i,
            sn: "+".into(),
            l: (*l).to_string(),
        });
    }
    let hunk = format!(
        "@@ -{start_line},{ol} +{start_line},{nl} @@",
        ol = old_lines.len(),
        nl = new_lines.len()
    );
    ToolView::Diff {
        files: vec![DiffFile {
            path: file_path.to_string(),
            add: new_lines.len(),
            rem: old_lines.len(),
            hunk,
            rows,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

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
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
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
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
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
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("not found"));
    }

    #[test]
    fn is_agent_only() {
        assert!(EditFileTool.agent_only());
    }

    #[tokio::test]
    async fn edit_rejects_empty_old_string() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "hello world").unwrap();

        let tool = EditFileTool;
        let input = serde_json::json!({
            "file_path": "test.txt",
            "old_string": "",
            "new_string": "replaced"
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("must not be empty"));
    }

    #[tokio::test]
    async fn edit_rejects_oversized_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.txt");
        // Create a file just over MAX_FILE_SIZE (10 MB).
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_FILE_SIZE + 1).unwrap();

        let tool = EditFileTool;
        let input = serde_json::json!({
            "file_path": "big.txt",
            "old_string": "find me",
            "new_string": "replaced"
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("too large"));
    }
}
