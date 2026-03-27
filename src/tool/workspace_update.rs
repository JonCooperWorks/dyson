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
         or 'append' to add to it. Changes are persisted immediately. \
         MEMORY.md and USER.md have character limits — the tool will report \
         current usage and reject writes that exceed the limit. Move overflow \
         to memory/notes/ (searchable via memory_search)."
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

        // Check character limits before writing.
        if let Some(limit) = ws.char_limit(&file) {
            let would_be_len = match mode {
                "set" => content.chars().count(),
                "append" => {
                    let existing = ws.get(&file).unwrap_or_default();
                    let extra = if existing.is_empty() || existing.ends_with('\n') { 0 } else { 1 };
                    existing.chars().count() + extra + content.chars().count()
                }
                _ => 0,
            };

            if would_be_len > limit {
                let current = ws.get(&file).map(|c| c.chars().count()).unwrap_or(0);
                return Ok(ToolOutput::error(format!(
                    "Would exceed character limit for '{file}': {would_be_len}/{limit} chars. \
                     Current usage: {current}/{limit}. Consolidate content or move overflow \
                     to memory/notes/ (searchable via memory_search)."
                )));
            }
        }

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

        // Report usage stats for files with limits.
        let final_len = ws.get(&file).map(|c| c.chars().count()).unwrap_or(0);
        let usage = match ws.char_limit(&file) {
            Some(limit) => format!(" [{final_len}/{limit} chars]"),
            None => String::new(),
        };
        Ok(ToolOutput::success(format!(
            "Updated '{file}' (mode: {mode}).{usage}"
        )))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::InMemoryWorkspace;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn make_ctx(ws: InMemoryWorkspace) -> ToolContext {
        let workspace: Box<dyn crate::workspace::Workspace> = Box::new(ws);
        ToolContext {
            working_dir: std::env::temp_dir(),
            env: std::collections::HashMap::new(),
            cancellation: tokio_util::sync::CancellationToken::new(),
            workspace: Some(Arc::new(RwLock::new(workspace))),
            depth: 0,
        }
    }

    #[tokio::test]
    async fn set_under_limit_succeeds_with_usage() {
        let ws = InMemoryWorkspace::new().with_limit("MEMORY.md", 100);
        let ctx = make_ctx(ws);
        let tool = WorkspaceUpdateTool;

        let result = tool
            .run(
                serde_json::json!({
                    "file": "MEMORY.md",
                    "content": "short content",
                    "mode": "set"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("/100 chars]"));
    }

    #[tokio::test]
    async fn set_over_limit_errors() {
        let ws = InMemoryWorkspace::new().with_limit("MEMORY.md", 10);
        let ctx = make_ctx(ws);
        let tool = WorkspaceUpdateTool;

        let result = tool
            .run(
                serde_json::json!({
                    "file": "MEMORY.md",
                    "content": "this content is way too long for the limit",
                    "mode": "set"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("Would exceed character limit"));
    }

    #[tokio::test]
    async fn append_over_limit_errors() {
        let ws = InMemoryWorkspace::new()
            .with_file("MEMORY.md", "existing content")
            .with_limit("MEMORY.md", 20);
        let ctx = make_ctx(ws);
        let tool = WorkspaceUpdateTool;

        let result = tool
            .run(
                serde_json::json!({
                    "file": "MEMORY.md",
                    "content": "more content that overflows",
                    "mode": "append"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("Would exceed character limit"));
    }

    #[tokio::test]
    async fn unlimited_file_has_no_usage_stats() {
        let ws = InMemoryWorkspace::new();
        let ctx = make_ctx(ws);
        let tool = WorkspaceUpdateTool;

        let result = tool
            .run(
                serde_json::json!({
                    "file": "SOUL.md",
                    "content": "anything goes",
                    "mode": "set"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(!result.content.contains("chars]"));
    }
}
