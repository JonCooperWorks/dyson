// ===========================================================================
// WorkspaceUpdateTool — update a file in the agent's workspace.
//
// Supports two modes:
//   - "set": replace the entire file content
//   - "append": append to the existing file (creates if missing)
//
// Automatically persists changes to disk after each update.
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;

use crate::error::DysonError;
use crate::tool::{Tool, ToolContext, ToolOutput};

pub struct WorkspaceUpdateTool;

#[async_trait]
impl Tool for WorkspaceUpdateTool {
    fn name(&self) -> &str {
        "workspace_update"
    }

    fn description(&self) -> &str {
        "Update a file in the agent's workspace. Use mode 'set' to replace content, \
         or 'append' to add to it. Changes are persisted immediately."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file": {
                    "type": "string",
                    "description": "File name to update, e.g. 'MEMORY.md' or 'SOUL.md'"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write or append"
                },
                "mode": {
                    "type": "string",
                    "enum": ["set", "append"],
                    "description": "Write mode: 'set' replaces the file, 'append' adds to it. Defaults to 'append'."
                }
            },
            "required": ["file", "content"]
        })
    }

    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let ws = ctx.workspace.as_ref().ok_or_else(|| {
            DysonError::tool("workspace_update", "no workspace configured")
        })?;

        let file = input["file"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let content = input["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let mode = input["mode"]
            .as_str()
            .unwrap_or("append");

        if file.is_empty() {
            return Ok(ToolOutput::error("file is required"));
        }
        if let Err(msg) = super::validate_workspace_path(&file) {
            return Ok(ToolOutput::error(msg));
        }
        if content.is_empty() {
            return Ok(ToolOutput::error("content is required"));
        }

        let mut ws = ws.write().await;

        match mode {
            "set" => ws.set(&file, &content),
            "append" => ws.append(&file, &content),
            other => {
                return Ok(ToolOutput::error(format!(
                    "unknown mode '{other}'. Use 'set' or 'append'."
                )));
            }
        }

        ws.save()?;

        Ok(ToolOutput::success(format!(
            "Updated '{file}' (mode: {mode})."
        )))
    }
}
