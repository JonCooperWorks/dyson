// ===========================================================================
// WorkspaceViewTool — view or list files in the agent's workspace.
//
// When called with a "file" parameter, returns that file's content.
// When called without "file", lists all available workspace files.
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;

use crate::error::DysonError;
use crate::tool::{Tool, ToolContext, ToolOutput};

pub struct WorkspaceViewTool;

#[async_trait]
impl Tool for WorkspaceViewTool {
    fn name(&self) -> &str {
        "workspace_view"
    }

    fn description(&self) -> &str {
        "View a file from the agent's workspace (SOUL.md, MEMORY.md, IDENTITY.md, journals, etc.), \
         or list all available workspace files when called without a file parameter."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file": {
                    "type": "string",
                    "description": "File name to view, e.g. 'SOUL.md' or 'memory/2026-03-19.md'. Omit to list all files."
                }
            }
        })
    }

    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let ws = ctx.workspace.as_ref().ok_or_else(|| {
            DysonError::tool("workspace_view", "no workspace configured")
        })?;

        let ws = ws.read().await;

        match input.get("file").and_then(|v| v.as_str()) {
            Some(file) => {
                match ws.get(file) {
                    Some(content) => Ok(ToolOutput::success(content)),
                    None => {
                        let files = ws.list_files();
                        Ok(ToolOutput::error(format!(
                            "File not found: '{file}'\n\nAvailable files:\n{}",
                            files.iter().map(|f| format!("  - {f}")).collect::<Vec<_>>().join("\n")
                        )))
                    }
                }
            }
            None => {
                let files = ws.list_files();
                if files.is_empty() {
                    Ok(ToolOutput::success("Workspace is empty."))
                } else {
                    Ok(ToolOutput::success(
                        files.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n")
                    ))
                }
            }
        }
    }
}
