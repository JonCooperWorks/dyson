// ===========================================================================
// ExportConversationTool — export the current conversation as ShareGPT JSON.
//
// This tool lets the agent export its own conversation history in ShareGPT
// format, producing training data for fine-tuning tool-calling models.
//
// The exported file is written to the workspace's programs directory (or CWD)
// and attached to the output so the controller can deliver it to the user.
//
// Usage by the agent:
//   export_conversation({ "path": "training/conv-001.json" })
//   export_conversation({})  // auto-generates timestamped filename
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;

use crate::error::DysonError;
use crate::export::sharegpt;
use crate::tool::{Tool, ToolContext, ToolOutput};

pub struct ExportConversationTool;

#[async_trait]
impl Tool for ExportConversationTool {
    fn name(&self) -> &str {
        "export_conversation"
    }

    fn description(&self) -> &str {
        "Export the current conversation as a ShareGPT-format JSON file for fine-tuning. \
         The exported file includes all messages (human, assistant, tool calls, tool results) \
         in the standard ShareGPT format used by Axolotl, LLaMA-Factory, and other training \
         frameworks. Optionally include the system prompt. The file is written to the \
         working directory."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Output file path (relative to working directory). \
                                    Defaults to 'sharegpt_export_<timestamp>.json' if not provided."
                },
                "include_system_prompt": {
                    "type": "boolean",
                    "description": "Whether to include the system prompt as the first turn. Defaults to false."
                },
                "id": {
                    "type": "string",
                    "description": "Optional conversation ID to include in the export."
                }
            }
        })
    }

    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        // Read messages from the workspace's conversation context.
        // The agent loop passes messages via a special metadata channel —
        // but since tools don't have direct access to the agent's message
        // history, we use a workspace-backed approach: the agent loop
        // snapshots messages into the tool context before calling this tool.
        //
        // Actually, the messages are not directly accessible from ToolContext.
        // This tool works with the chat history that's been serialized.
        // The agent should call this by providing messages through the
        // workspace or we read from the chat_history on disk.
        //
        // Design decision: This tool reads from the DiskChatHistory path.
        // The agent loop serializes messages to disk via ChatHistory::save()
        // after each turn, so the most recent state is always available.

        let path_str = input["path"].as_str().unwrap_or("");
        let include_system = input["include_system_prompt"].as_bool().unwrap_or(false);
        let id = input["id"].as_str().map(String::from);

        // Generate default filename with timestamp.
        let output_filename = if path_str.is_empty() {
            let epoch = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            format!("sharegpt_export_{epoch}.json")
        } else {
            path_str.to_string()
        };

        let output_path = ctx.working_dir.join(&output_filename);

        // Ensure parent directory exists.
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                DysonError::tool(
                    "export_conversation",
                    format!("cannot create directory: {e}"),
                )
            })?;
        }

        // Read messages from chat history files in the workspace.
        // Look for the most recent chat history JSON file.
        let messages = self.find_messages(ctx).await?;

        if messages.is_empty() {
            return Ok(ToolOutput::error(
                "No conversation messages found. The conversation may not have been saved yet.",
            ));
        }

        // Get system prompt from workspace if requested.
        let system_prompt = if include_system {
            if let Some(ref ws) = ctx.workspace {
                let ws = ws.read().await;
                Some(ws.system_prompt())
            } else {
                None
            }
        } else {
            None
        };

        let conversation = sharegpt::to_sharegpt(&messages, system_prompt.as_deref(), id);

        let conversations = vec![conversation];
        let json = sharegpt::to_sharegpt_json(&conversations)?;

        std::fs::write(&output_path, &json).map_err(|e| {
            DysonError::tool(
                "export_conversation",
                format!("failed to write {}: {e}", output_path.display()),
            )
        })?;

        let turn_count = conversations[0].conversations.len();
        Ok(ToolOutput::success(format!(
            "Exported {turn_count} turns to '{output_filename}' (ShareGPT format, {} bytes).",
            json.len()
        ))
        .with_file(&output_path))
    }
}

impl ExportConversationTool {
    /// Find the most recent conversation messages.
    ///
    /// Searches the workspace's chat history directory for JSON files
    /// containing serialized message arrays.
    async fn find_messages(
        &self,
        ctx: &ToolContext,
    ) -> crate::Result<Vec<crate::message::Message>> {
        // Strategy: look for chat history files in common locations.
        // DiskChatHistory stores files as `<chat_id>.json` in the history dir.
        let mut candidates = vec![
            ctx.working_dir.join(".dyson/chat_history"),
            ctx.working_dir.join("chat_history"),
        ];
        if let Ok(home) = std::env::var("HOME") {
            candidates.push(std::path::PathBuf::from(home).join(".dyson/chat_history"));
        }

        for dir in &candidates {
            if !dir.is_dir() {
                continue;
            }

            // Find the most recently modified JSON file.
            let mut entries: Vec<_> = std::fs::read_dir(dir)
                .map_err(DysonError::Io)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
                .collect();

            entries.sort_by_key(|e| {
                e.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            });

            if let Some(entry) = entries.last() {
                let content = std::fs::read_to_string(entry.path()).map_err(DysonError::Io)?;
                if let Ok(messages) = serde_json::from_str::<Vec<crate::message::Message>>(&content)
                {
                    return Ok(messages);
                }
            }
        }

        // No chat history found — return empty.
        // The agent can still use this tool by first saving via chat_history.
        Ok(vec![])
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, Message};
    use std::collections::HashMap;

    fn make_ctx_with_history(messages: &[Message]) -> (ToolContext, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let history_dir = tmp.path().join(".dyson/chat_history");
        std::fs::create_dir_all(&history_dir).unwrap();

        // Write messages to a chat history file.
        let json = serde_json::to_string_pretty(messages).unwrap();
        std::fs::write(history_dir.join("test-chat.json"), json).unwrap();

        let ctx = ToolContext {
            working_dir: tmp.path().to_path_buf(),
            env: HashMap::new(),
            cancellation: tokio_util::sync::CancellationToken::new(),
            workspace: None,
            depth: 0,
        };

        (ctx, tmp)
    }

    #[tokio::test]
    async fn export_simple_conversation() {
        let messages = vec![
            Message::user("Hello"),
            Message::assistant(vec![ContentBlock::Text {
                text: "Hi there!".into(),
            }]),
        ];

        let (ctx, _tmp) = make_ctx_with_history(&messages);
        let tool = ExportConversationTool;

        let result = tool
            .run(json!({"path": "test_export.json"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error, "Error: {}", result.content);
        assert!(result.content.contains("2 turns"));
        assert!(result.content.contains("ShareGPT format"));

        // Verify the file was created and is valid JSON.
        let exported = std::fs::read_to_string(ctx.working_dir.join("test_export.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&exported).unwrap();
        assert!(parsed.is_array());
        let convs = parsed.as_array().unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0]["conversations"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn export_with_tool_calls() {
        let messages = vec![
            Message::user("List files"),
            Message::assistant(vec![
                ContentBlock::Text {
                    text: "Let me check.".into(),
                },
                ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"}),
                },
            ]),
            Message::tool_result("c1", "file.txt", false),
            Message::assistant(vec![ContentBlock::Text {
                text: "Found file.txt".into(),
            }]),
        ];

        let (ctx, _tmp) = make_ctx_with_history(&messages);
        let tool = ExportConversationTool;

        let result = tool
            .run(json!({"path": "tools_export.json"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("4 turns"));

        let exported = std::fs::read_to_string(ctx.working_dir.join("tools_export.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&exported).unwrap();
        let turns = parsed[0]["conversations"].as_array().unwrap();

        // Verify tool call format
        assert_eq!(turns[1]["from"], "gpt");
        assert!(turns[1]["value"].as_str().unwrap().contains("<tool_call>"));
        assert_eq!(turns[2]["from"], "tool");
    }

    #[tokio::test]
    async fn export_auto_generates_filename() {
        let messages = vec![Message::user("Test")];
        let (ctx, _tmp) = make_ctx_with_history(&messages);
        let tool = ExportConversationTool;

        let result = tool.run(json!({}), &ctx).await.unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("sharegpt_export_"));
    }

    #[tokio::test]
    async fn export_empty_history_returns_error() {
        let ctx = ToolContext {
            working_dir: std::env::temp_dir().join("dyson-empty-export-test"),
            env: HashMap::new(),
            cancellation: tokio_util::sync::CancellationToken::new(),
            workspace: None,
            depth: 0,
        };

        let tool = ExportConversationTool;
        let result = tool.run(json!({}), &ctx).await.unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("No conversation messages"));
    }

    #[tokio::test]
    async fn export_with_id() {
        let messages = vec![Message::user("Hi")];
        let (ctx, _tmp) = make_ctx_with_history(&messages);
        let tool = ExportConversationTool;

        let result = tool
            .run(json!({"path": "id_export.json", "id": "conv-42"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        let exported = std::fs::read_to_string(ctx.working_dir.join("id_export.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&exported).unwrap();
        assert_eq!(parsed[0]["id"], "conv-42");
    }

    #[tokio::test]
    async fn exported_file_is_attached() {
        let messages = vec![Message::user("Hi")];
        let (ctx, _tmp) = make_ctx_with_history(&messages);
        let tool = ExportConversationTool;

        let result = tool
            .run(json!({"path": "attached.json"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(result.files.len(), 1);
        assert!(result.files[0].ends_with("attached.json"));
    }
}
