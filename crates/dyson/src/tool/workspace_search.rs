// ===========================================================================
// WorkspaceSearchTool — search across workspace files for a pattern.
//
// Case-insensitive substring search across all loaded workspace files.
// Returns matching filenames and their matching lines.
// ===========================================================================

use std::fmt::Write;

use async_trait::async_trait;
use serde_json::json;

use crate::tool::{Tool, ToolContext, ToolOutput};

pub struct WorkspaceSearchTool;

#[async_trait]
impl Tool for WorkspaceSearchTool {
    fn name(&self) -> &str {
        "workspace_search"
    }

    fn description(&self) -> &str {
        "Search across all workspace files for a pattern (regex supported). \
         Returns matching filenames and lines. Case-insensitive."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for (case-insensitive). Falls back to literal substring if not valid regex."
                }
            },
            "required": ["pattern"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let ws = ctx.workspace("workspace_search")?;

        let pattern = input["pattern"].as_str().unwrap_or("").to_string();

        if pattern.is_empty() {
            return Ok(ToolOutput::error("pattern is required"));
        }

        let results = ws.read().await.search(&pattern);

        if results.is_empty() {
            Ok(ToolOutput::success(format!("No matches for '{pattern}'.")))
        } else {
            let mut output = String::new();
            for (file, lines) in &results {
                writeln!(&mut output, "### {file}").unwrap();
                for line in lines {
                    writeln!(&mut output, "  {line}").unwrap();
                }
                output.push('\n');
            }
            Ok(ToolOutput::success(output))
        }
    }
}
