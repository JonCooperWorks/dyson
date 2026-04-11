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
         MEMORY.md and USER.md have a fuzzy soft character target plus a hard \
         ceiling — writes between the two land with an 'over soft target' \
         warning (allowed when the extra chars are signal), writes above the \
         ceiling are rejected. Move overflow to memory/notes/ (searchable via \
         memory_search) when even the ceiling is tight."
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

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let ws = ctx.workspace("workspace_update")?;

        let file = input["file"].as_str().unwrap_or("").to_string();
        let content = input["content"].as_str().unwrap_or("").to_string();
        let mode = input["mode"].as_str().unwrap_or("append");

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

        // Fuzzy size check: reject only above the hard ceiling, not the
        // soft target.  Writes between soft target and ceiling land with
        // an "over soft target" note in the success message so the
        // curator knows it's using overflow headroom.
        let soft_target = ws.char_limit(&file);
        let ceiling = ws.char_ceiling(&file);
        if let Some(ceil) = ceiling {
            let existing = ws.get(&file).unwrap_or_default();
            let existing_char_count = existing.chars().count();
            let would_be_len = match mode {
                "set" => content.chars().count(),
                "append" => {
                    let extra = if existing.is_empty() || existing.ends_with('\n') {
                        0
                    } else {
                        1
                    };
                    existing_char_count + extra + content.chars().count()
                }
                _ => 0,
            };

            if would_be_len > ceil {
                let target = soft_target.unwrap_or(ceil);
                return Ok(ToolOutput::error(format!(
                    "Would exceed hard ceiling for '{file}': {would_be_len} chars \
                     (soft target {target}, ceiling {ceil}). Current usage: \
                     {existing_char_count}. Apply the Keep/Refine/Discard judgment \
                     to prune noise, or move overflow to memory/notes/ \
                     (searchable via memory_search)."
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

        // Report usage stats for files with a soft target.
        let final_len = ws.get(&file).map(|c| c.chars().count()).unwrap_or(0);
        let usage = match (soft_target, ceiling) {
            (Some(target), Some(ceil)) if final_len > target => {
                format!(" [{final_len}/{target} chars — over soft target, within ceiling {ceil}]")
            }
            (Some(target), _) => format!(" [{final_len}/{target} chars]"),
            _ => String::new(),
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

    #[tokio::test]
    async fn set_under_soft_target_succeeds_with_usage() {
        let ws = InMemoryWorkspace::new().with_limit("MEMORY.md", 100);
        let ctx = ToolContext::for_test_with_workspace(ws);
        let tool = WorkspaceUpdateTool;

        let result = tool
            .run(
                &serde_json::json!({
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
        assert!(!result.content.contains("over soft target"));
    }

    #[tokio::test]
    async fn set_in_overflow_band_succeeds_with_warning() {
        // target 100, factor 1.35 → ceiling 135.  120 chars should land
        // with an "over soft target" warning but still succeed.
        let ws = InMemoryWorkspace::new()
            .with_overflow_factor(1.35)
            .with_limit("MEMORY.md", 100);
        let ctx = ToolContext::for_test_with_workspace(ws);
        let tool = WorkspaceUpdateTool;

        let payload = "x".repeat(120);
        let result = tool
            .run(
                &serde_json::json!({
                    "file": "MEMORY.md",
                    "content": payload,
                    "mode": "set"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error, "overflow band should succeed");
        assert!(result.content.contains("over soft target"));
        assert!(result.content.contains("ceiling 135"));
    }

    #[tokio::test]
    async fn set_over_ceiling_errors() {
        // target 10, factor 1.35 → ceiling 14.  Content of 43 chars is
        // well above the ceiling.
        let ws = InMemoryWorkspace::new()
            .with_overflow_factor(1.35)
            .with_limit("MEMORY.md", 10);
        let ctx = ToolContext::for_test_with_workspace(ws);
        let tool = WorkspaceUpdateTool;

        let result = tool
            .run(
                &serde_json::json!({
                    "file": "MEMORY.md",
                    "content": "this content is way too long for the limit",
                    "mode": "set"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("hard ceiling"));
    }

    #[tokio::test]
    async fn append_over_ceiling_errors() {
        let ws = InMemoryWorkspace::new()
            .with_file("MEMORY.md", "existing content")
            .with_overflow_factor(1.35)
            .with_limit("MEMORY.md", 20);
        let ctx = ToolContext::for_test_with_workspace(ws);
        let tool = WorkspaceUpdateTool;

        let result = tool
            .run(
                &serde_json::json!({
                    "file": "MEMORY.md",
                    "content": "more content that overflows",
                    "mode": "append"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("hard ceiling"));
    }

    #[tokio::test]
    async fn unlimited_file_has_no_usage_stats() {
        let ws = InMemoryWorkspace::new();
        let ctx = ToolContext::for_test_with_workspace(ws);
        let tool = WorkspaceUpdateTool;

        let result = tool
            .run(
                &serde_json::json!({
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
