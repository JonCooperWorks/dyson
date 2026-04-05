// ===========================================================================
// SearchFiles tool — regex content search across files.
// ===========================================================================

use std::io::{BufRead, BufReader};

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};
use crate::util::MAX_OUTPUT_BYTES;

/// Maximum number of matching lines to collect.
const MAX_MATCHES: usize = 500;

pub struct SearchFilesTool;

#[async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }

    fn description(&self) -> &str {
        "Search file contents using a regex pattern. Returns matching lines \
         with file paths and line numbers (like grep). Respects .gitignore. \
         Use `include` to filter by file glob (e.g. '*.rs')."
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
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (relative to working directory). Defaults to working directory."
                },
                "include": {
                    "type": "string",
                    "description": "Glob pattern to filter which files to search (e.g. '*.rs', '*.py')"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let pattern_str = input["pattern"]
            .as_str()
            .ok_or_else(|| DysonError::tool("search_files", "missing or invalid 'pattern'"))?;

        let regex = match regex::Regex::new(pattern_str) {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::error(format!("invalid regex: {e}"))),
        };

        let search_dir = if let Some(sub) = input["path"].as_str() {
            // Validate the path doesn't escape the working directory.
            match super::resolve_and_validate_path(&ctx.working_dir, sub) {
                Ok(resolved) => resolved,
                Err(e) => return Ok(ToolOutput::error(e)),
            }
        } else {
            ctx.working_dir.clone()
        };

        if !search_dir.exists() {
            return Ok(ToolOutput::error(format!(
                "directory does not exist: '{}'",
                search_dir.display()
            )));
        }

        let include_glob = input["include"].as_str();

        // Build the directory walker using the `ignore` crate,
        // which respects .gitignore automatically.
        let mut builder = ignore::WalkBuilder::new(&search_dir);
        builder.hidden(false); // search hidden files too
        builder.git_ignore(true);
        builder.git_global(true);

        if let Some(glob) = include_glob {
            // Add a file type glob filter.
            let mut types_builder = ignore::types::TypesBuilder::new();
            types_builder.add("filter", glob).ok();
            types_builder.select("filter");
            if let Ok(types) = types_builder.build() {
                builder.types(types);
            }
        }

        let working_dir_canon = ctx
            .working_dir
            .canonicalize()
            .unwrap_or_else(|_| ctx.working_dir.clone());

        // Walk and search — this is CPU-bound, so run in a blocking task.
        let regex_clone = regex.clone();
        let results = tokio::task::spawn_blocking(move || {
            let mut matches = Vec::new();
            let mut total_bytes = 0usize;

            for entry in builder.build().flatten() {
                if matches.len() >= MAX_MATCHES || total_bytes >= MAX_OUTPUT_BYTES {
                    break;
                }

                let path = entry.path();
                if !path.is_file() {
                    continue;
                }

                let file = match std::fs::File::open(path) {
                    Ok(f) => f,
                    Err(_) => continue,
                };

                // Compute relative path without a per-file canonicalize()
                // syscall — strip_prefix on the walk-root-relative path is
                // a pure memory operation.
                let rel_path = path
                    .strip_prefix(&working_dir_canon)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| path.to_string_lossy().to_string());

                let reader = BufReader::new(file);
                for (line_num, line_result) in reader.lines().enumerate() {
                    if matches.len() >= MAX_MATCHES || total_bytes >= MAX_OUTPUT_BYTES {
                        break;
                    }

                    let line = match line_result {
                        Ok(l) => l,
                        Err(_) => break, // Binary file or encoding error
                    };

                    if regex_clone.is_match(&line) {
                        let entry = format!("{}:{}: {}", rel_path, line_num + 1, line);
                        total_bytes += entry.len() + 1;
                        matches.push(entry);
                    }
                }
            }

            matches
        })
        .await
        .map_err(|e| DysonError::tool("search_files", format!("search task failed: {e}")))?;

        if results.is_empty() {
            return Ok(ToolOutput::success("No matches found."));
        }

        let mut output = results.join("\n");
        if results.len() >= MAX_MATCHES {
            output.push_str(&format!("\n\n... (truncated at {MAX_MATCHES} matches)"));
        }

        Ok(ToolOutput::success(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    #[tokio::test]
    async fn search_finds_matches() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn hello() {}\nfn world() {}\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "no match here\n").unwrap();

        let tool = SearchFilesTool;
        let input = serde_json::json!({"pattern": "fn \\w+"});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("fn hello"));
        assert!(output.content.contains("fn world"));
    }

    #[tokio::test]
    async fn search_no_matches() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "hello world\n").unwrap();

        let tool = SearchFilesTool;
        let input = serde_json::json!({"pattern": "zzzzz"});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("No matches"));
    }

    #[tokio::test]
    async fn search_invalid_regex() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SearchFilesTool;
        let input = serde_json::json!({"pattern": "[invalid"});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(output.is_error);
    }

    #[test]
    fn is_agent_only() {
        assert!(SearchFilesTool.agent_only());
    }
}
