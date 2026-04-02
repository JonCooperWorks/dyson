// ===========================================================================
// WorkspaceSearchTool — search across workspace files for a pattern.
//
// Case-insensitive substring search across all loaded workspace files.
// Returns matching filenames and their matching lines.
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;

use crate::error::DysonError;
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
        let ws = ctx
            .workspace
            .as_ref()
            .ok_or_else(|| DysonError::tool("workspace_search", "no workspace configured"))?;

        let pattern = input["pattern"].as_str().unwrap_or("").to_string();

        if pattern.is_empty() {
            return Ok(ToolOutput::error("pattern is required"));
        }

        let ws = ws.read().await;
        let results = ws.search(&pattern);

        if results.is_empty() {
            Ok(ToolOutput::success(format!("No matches for '{pattern}'.")))
        } else {
            let mut output = String::new();
            for (file, lines) in &results {
                output.push_str(&format!("### {file}\n"));
                for line in lines {
                    output.push_str(&format!("  {line}\n"));
                }
                output.push('\n');
            }
            Ok(ToolOutput::success(output))
        }
    }
}
