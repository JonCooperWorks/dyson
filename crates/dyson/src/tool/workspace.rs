// ===========================================================================
// WorkspaceTool — unified view/list/search/update for agent workspace files.
//
// Four operations dispatched via the `op` field:
//   - view:   read a workspace file by name
//   - list:   list all workspace files
//   - search: regex/substring search across all workspace files
//   - update: set or append a workspace file (with soft-target/hard-ceiling
//             enforcement on files that declare char limits)
//
// Everything goes through the `Workspace` trait so in-memory and disk-backed
// implementations behave identically.  Callers use workspace-relative file
// names ("MEMORY.md", "memory/2026-03-19.md") — not filesystem paths —
// because the workspace owns layout and caching.
// ===========================================================================

use std::fmt::Write;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::tool::{Tool, ToolContext, ToolOutput};
use crate::workspace::WorkspaceHandle;

pub struct WorkspaceTool;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum Op {
    View {
        file: String,
    },
    List,
    Search {
        pattern: String,
    },
    Update {
        file: String,
        content: String,
        #[serde(default)]
        mode: UpdateMode,
    },
}

#[derive(Deserialize, Default, Copy, Clone)]
#[serde(rename_all = "lowercase")]
enum UpdateMode {
    Set,
    #[default]
    Append,
}

impl UpdateMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Append => "append",
        }
    }
}

#[async_trait]
impl Tool for WorkspaceTool {
    fn name(&self) -> &str {
        "workspace"
    }

    fn description(&self) -> &str {
        "Read, search, or update the agent's workspace (SOUL.md, MEMORY.md, \
         IDENTITY.md, journals, etc.). Four operations via `op`: \
         'view' (read one file), 'list' (enumerate files), \
         'search' (regex/substring across all files, case-insensitive), \
         'update' (write or append; default mode is 'append'). \
         MEMORY.md and USER.md enforce a fuzzy soft character target plus a \
         hard ceiling — writes between the two land with an 'over soft target' \
         warning, writes above the ceiling are rejected. Move overflow to \
         memory/notes/ (searchable via memory_search) when even the ceiling is tight."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["view", "list", "search", "update"],
                    "description": "Operation to perform."
                },
                "file": {
                    "type": "string",
                    "description": "(view, update) Workspace file name, e.g. 'SOUL.md' or 'memory/2026-03-19.md'."
                },
                "pattern": {
                    "type": "string",
                    "description": "(search) Regex pattern to search for (case-insensitive). Falls back to literal substring if not valid regex."
                },
                "content": {
                    "type": "string",
                    "description": "(update) Content to write or append."
                },
                "mode": {
                    "type": "string",
                    "enum": ["set", "append"],
                    "description": "(update, optional) 'set' replaces the file, 'append' adds to it. Defaults to 'append'."
                }
            },
            "required": ["op"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let ws = ctx.workspace("workspace")?;
        let op: Op = match serde_json::from_value(input.clone()) {
            Ok(op) => op,
            Err(e) => return Ok(ToolOutput::error(format!("invalid input: {e}"))),
        };

        match op {
            Op::View { file } => view(ws, &file).await,
            Op::List => list(ws).await,
            Op::Search { pattern } => search(ws, &pattern).await,
            Op::Update {
                file,
                content,
                mode,
            } => update(ws, &file, &content, mode).await,
        }
    }
}

async fn view(ws: &WorkspaceHandle, file: &str) -> crate::Result<ToolOutput> {
    if let Err(msg) = super::validate_workspace_path(file) {
        return Ok(ToolOutput::error(msg));
    }
    let ws = ws.read().await;
    match ws.get(file) {
        Some(content) => Ok(ToolOutput::success(content)),
        None => {
            let available = ws
                .list_files()
                .iter()
                .map(|f| format!("  - {f}"))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(ToolOutput::error(format!(
                "File not found: '{file}'\n\nAvailable files:\n{available}"
            )))
        }
    }
}

async fn list(ws: &WorkspaceHandle) -> crate::Result<ToolOutput> {
    let files = ws.read().await.list_files();
    if files.is_empty() {
        Ok(ToolOutput::success("Workspace is empty."))
    } else {
        Ok(ToolOutput::success(
            files
                .iter()
                .map(|f| format!("- {f}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}

async fn search(ws: &WorkspaceHandle, pattern: &str) -> crate::Result<ToolOutput> {
    let results = ws.read().await.search(pattern);
    if results.is_empty() {
        return Ok(ToolOutput::success(format!("No matches for '{pattern}'.")));
    }
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

async fn update(
    ws: &WorkspaceHandle,
    file: &str,
    content: &str,
    mode: UpdateMode,
) -> crate::Result<ToolOutput> {
    if let Err(msg) = super::validate_workspace_path(file) {
        return Ok(ToolOutput::error(msg));
    }

    let mut ws = ws.write().await;

    // Fuzzy size check: reject only above the hard ceiling, not the soft
    // target.  Writes between soft target and ceiling land with an "over
    // soft target" note in the success message so the curator knows it's
    // using overflow headroom.
    let soft_target = ws.char_limit(file);
    let ceiling = ws.char_ceiling(file);
    if let Some(ceil) = ceiling {
        let existing = ws.get(file).unwrap_or_default();
        let existing_len = existing.chars().count();
        let would_be_len = match mode {
            UpdateMode::Set => content.chars().count(),
            UpdateMode::Append => {
                let separator = usize::from(!existing.is_empty() && !existing.ends_with('\n'));
                existing_len + separator + content.chars().count()
            }
        };
        if would_be_len > ceil {
            let target = soft_target.unwrap_or(ceil);
            return Ok(ToolOutput::error(format!(
                "Would exceed hard ceiling for '{file}': {would_be_len} chars \
                 (soft target {target}, ceiling {ceil}). Current usage: \
                 {existing_len}. Apply the Keep/Refine/Discard judgment \
                 to prune noise, or move overflow to memory/notes/ \
                 (searchable via memory_search)."
            )));
        }
    }

    match mode {
        UpdateMode::Set => ws.set(file, content),
        UpdateMode::Append => ws.append(file, content),
    }
    ws.save()?;

    let final_len = ws.get(file).map(|c| c.chars().count()).unwrap_or(0);
    let usage = match (soft_target, ceiling) {
        (Some(target), Some(ceil)) if final_len > target => {
            format!(" [{final_len}/{target} chars — over soft target, within ceiling {ceil}]")
        }
        (Some(target), _) => format!(" [{final_len}/{target} chars]"),
        _ => String::new(),
    };
    Ok(ToolOutput::success(format!(
        "Updated '{file}' (mode: {}).{usage}",
        mode.as_str()
    )))
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::InMemoryWorkspace;

    async fn run(input: serde_json::Value, ws: InMemoryWorkspace) -> ToolOutput {
        let ctx = ToolContext::for_test_with_workspace(ws);
        WorkspaceTool.run(&input, &ctx).await.unwrap()
    }

    #[tokio::test]
    async fn op_required() {
        let out = run(json!({}), InMemoryWorkspace::new()).await;
        assert!(out.is_error);
        assert!(out.content.contains("invalid input"));
    }

    #[tokio::test]
    async fn unknown_op_rejected() {
        let out = run(json!({"op": "bogus"}), InMemoryWorkspace::new()).await;
        assert!(out.is_error);
        assert!(out.content.contains("invalid input"));
    }

    #[tokio::test]
    async fn view_returns_file_content() {
        let out = run(
            json!({"op": "view", "file": "SOUL.md"}),
            InMemoryWorkspace::new().with_file("SOUL.md", "i am dyson"),
        )
        .await;
        assert!(!out.is_error);
        assert_eq!(out.content, "i am dyson");
    }

    #[tokio::test]
    async fn view_missing_file_lists_available() {
        let out = run(
            json!({"op": "view", "file": "GHOST.md"}),
            InMemoryWorkspace::new().with_file("SOUL.md", "x"),
        )
        .await;
        assert!(out.is_error);
        assert!(out.content.contains("File not found"));
        assert!(out.content.contains("SOUL.md"));
    }

    #[tokio::test]
    async fn list_enumerates_files() {
        let out = run(
            json!({"op": "list"}),
            InMemoryWorkspace::new()
                .with_file("SOUL.md", "x")
                .with_file("MEMORY.md", "y"),
        )
        .await;
        assert!(!out.is_error);
        assert!(out.content.contains("SOUL.md"));
        assert!(out.content.contains("MEMORY.md"));
    }

    #[tokio::test]
    async fn search_finds_matches() {
        let out = run(
            json!({"op": "search", "pattern": "receipts"}),
            InMemoryWorkspace::new().with_file("MEMORY.md", "keep the receipts\n"),
        )
        .await;
        assert!(!out.is_error);
        assert!(out.content.contains("MEMORY.md"));
        assert!(out.content.contains("receipts"));
    }

    #[tokio::test]
    async fn set_under_soft_target_succeeds_with_usage() {
        let out = run(
            json!({"op": "update", "file": "MEMORY.md", "content": "short content", "mode": "set"}),
            InMemoryWorkspace::new().with_limit("MEMORY.md", 100),
        )
        .await;
        assert!(!out.is_error);
        assert!(out.content.contains("/100 chars]"));
        assert!(!out.content.contains("over soft target"));
    }

    #[tokio::test]
    async fn set_in_overflow_band_succeeds_with_warning() {
        let out = run(
            json!({"op": "update", "file": "MEMORY.md", "content": "x".repeat(120), "mode": "set"}),
            InMemoryWorkspace::new()
                .with_overflow_factor(1.35)
                .with_limit("MEMORY.md", 100),
        )
        .await;
        assert!(!out.is_error, "overflow band should succeed");
        assert!(out.content.contains("over soft target"));
        assert!(out.content.contains("ceiling 135"));
    }

    #[tokio::test]
    async fn set_over_ceiling_errors() {
        let out = run(
            json!({"op": "update", "file": "MEMORY.md", "content": "this content is way too long for the limit", "mode": "set"}),
            InMemoryWorkspace::new()
                .with_overflow_factor(1.35)
                .with_limit("MEMORY.md", 10),
        )
        .await;
        assert!(out.is_error);
        assert!(out.content.contains("hard ceiling"));
    }

    #[tokio::test]
    async fn append_over_ceiling_errors() {
        let out = run(
            json!({"op": "update", "file": "MEMORY.md", "content": "more content that overflows", "mode": "append"}),
            InMemoryWorkspace::new()
                .with_file("MEMORY.md", "existing content")
                .with_overflow_factor(1.35)
                .with_limit("MEMORY.md", 20),
        )
        .await;
        assert!(out.is_error);
        assert!(out.content.contains("hard ceiling"));
    }

    #[tokio::test]
    async fn unlimited_file_has_no_usage_stats() {
        let out = run(
            json!({"op": "update", "file": "SOUL.md", "content": "anything goes", "mode": "set"}),
            InMemoryWorkspace::new(),
        )
        .await;
        assert!(!out.is_error);
        assert!(!out.content.contains("chars]"));
    }

    #[tokio::test]
    async fn update_default_mode_is_append() {
        let out = run(
            json!({"op": "update", "file": "MEMORY.md", "content": " world"}),
            InMemoryWorkspace::new().with_file("MEMORY.md", "hello"),
        )
        .await;
        assert!(!out.is_error);
        assert!(out.content.contains("mode: append"));
    }

    #[tokio::test]
    async fn update_missing_file_rejected_by_serde() {
        let out = run(
            json!({"op": "update", "content": "x"}),
            InMemoryWorkspace::new(),
        )
        .await;
        assert!(out.is_error);
        assert!(out.content.contains("invalid input"));
    }

    #[tokio::test]
    async fn search_missing_pattern_rejected_by_serde() {
        let out = run(json!({"op": "search"}), InMemoryWorkspace::new()).await;
        assert!(out.is_error);
        assert!(out.content.contains("invalid input"));
    }
}
