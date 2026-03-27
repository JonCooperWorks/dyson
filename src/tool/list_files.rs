// ===========================================================================
// ListFiles tool — glob-based file discovery.
// ===========================================================================

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};

/// Maximum number of results to return.
const MAX_RESULTS: usize = 1000;

pub struct ListFilesTool;

#[async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> &str {
        "list_files"
    }

    fn description(&self) -> &str {
        "List files matching a glob pattern (e.g. '**/*.rs', 'src/*.py'). \
         Returns relative paths, one per line. Capped at 1000 results."
    }

    fn agent_only(&self) -> bool {
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match files (e.g. '**/*.rs', 'src/**/*.ts')"
                },
                "path": {
                    "type": "string",
                    "description": "Subdirectory to search in (relative to working directory). Defaults to working directory."
                }
            },
            "required": ["pattern"]
        })
    }

    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let pattern = input["pattern"]
            .as_str()
            .ok_or_else(|| DysonError::tool("list_files", "missing or invalid 'pattern'"))?;

        let base_dir = if let Some(sub) = input["path"].as_str() {
            ctx.working_dir.join(sub)
        } else {
            ctx.working_dir.clone()
        };

        if !base_dir.exists() {
            return Ok(ToolOutput::error(format!(
                "directory does not exist: '{}'",
                base_dir.display()
            )));
        }

        // Build the full glob pattern.
        let full_pattern = base_dir.join(pattern);
        let full_pattern_str = full_pattern.to_string_lossy();

        let entries = match glob::glob(&full_pattern_str) {
            Ok(paths) => paths,
            Err(e) => {
                return Ok(ToolOutput::error(format!("invalid glob pattern: {e}")));
            }
        };

        let working_dir_canon = ctx
            .working_dir
            .canonicalize()
            .unwrap_or_else(|_| ctx.working_dir.clone());

        let mut results = Vec::new();
        for entry in entries {
            if results.len() >= MAX_RESULTS {
                break;
            }
            match entry {
                Ok(path) => {
                    // Make path relative to working dir.
                    let rel = if let Ok(canon) = path.canonicalize() {
                        canon
                            .strip_prefix(&working_dir_canon)
                            .map(|p| p.to_path_buf())
                            .unwrap_or_else(|_| path.clone())
                    } else {
                        path.strip_prefix(&ctx.working_dir)
                            .map(|p| p.to_path_buf())
                            .unwrap_or(path)
                    };
                    results.push(rel.to_string_lossy().to_string());
                }
                Err(e) => {
                    tracing::debug!(error = %e, "glob entry error — skipping");
                }
            }
        }

        if results.is_empty() {
            return Ok(ToolOutput::success("No files matched the pattern."));
        }

        let mut output = results.join("\n");
        if results.len() >= MAX_RESULTS {
            output.push_str(&format!("\n\n... (truncated at {MAX_RESULTS} results)"));
        }

        Ok(ToolOutput::success(output))
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
    async fn list_files_glob() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "").unwrap();

        let tool = ListFilesTool;
        let input = serde_json::json!({"pattern": "*.rs"});
        let output = tool.run(input, &test_ctx(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("a.rs"));
        assert!(output.content.contains("b.rs"));
        assert!(!output.content.contains("c.txt"));
    }

    #[tokio::test]
    async fn invalid_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ListFilesTool;
        let input = serde_json::json!({"pattern": "[invalid"});
        let output = tool.run(input, &test_ctx(tmp.path())).await.unwrap();
        assert!(output.is_error);
    }

    #[test]
    fn is_agent_only() {
        assert!(ListFilesTool.agent_only());
    }
}
