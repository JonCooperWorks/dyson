use std::fmt::Write as _;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};
use crate::util::truncate_output;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtefactSummary {
    pub id: String,
    pub chat_id: String,
    pub kind: String,
    pub title: String,
    pub bytes: usize,
    pub created_at: u64,
    pub tool_use_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtefactRecord {
    pub id: String,
    pub chat_id: String,
    pub kind: String,
    pub title: String,
    pub content: String,
    pub mime_type: String,
    pub bytes: usize,
    pub created_at: u64,
    pub tool_use_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

pub trait ArtefactReader: Send + Sync {
    fn list(&self, chat_id: &str, limit: usize) -> Result<Vec<ArtefactSummary>>;
    fn read(&self, chat_id: &str, id: &str) -> Result<Option<ArtefactRecord>>;
}

pub struct ArtefactsTool;

#[async_trait]
impl Tool for ArtefactsTool {
    fn name(&self) -> &str {
        "artefacts"
    }

    fn description(&self) -> &str {
        "List or read artefacts produced in the current chat. Use this when the user asks \
         to inspect an artefact, report, generated image record, or document-shaped output \
         that was emitted as an artefact instead of regular chat text."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["list", "read"],
                    "description": "Use 'list' to discover artefacts in the current chat, or 'read' to load one artefact body. Defaults to 'read' when id is present, otherwise 'list'."
                },
                "id": {
                    "type": "string",
                    "description": "Artefact id to read, such as 'a1'. Required for operation='read'."
                },
                "limit": {
                    "type": "integer",
                    "description": "For list: maximum artefacts to return, default 20, max 100. For read: maximum content lines to include."
                },
                "offset": {
                    "type": "integer",
                    "description": "For read: 1-based content line to start at. Defaults to 1."
                }
            }
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let Some(reader) = ctx.artefacts.as_ref() else {
            return Ok(ToolOutput::error(
                "artefact access is not configured for this controller",
            ));
        };
        let Some(chat_id) = ctx.current_chat_id.as_deref() else {
            return Ok(ToolOutput::error(
                "artefact access is missing the current chat id",
            ));
        };

        let operation = input["operation"].as_str().unwrap_or_else(|| {
            if input.get("id").and_then(|v| v.as_str()).is_some() {
                "read"
            } else {
                "list"
            }
        });

        match operation {
            "list" => {
                let limit = bounded_limit(input["limit"].as_u64(), 20, 100);
                let artefacts = reader.list(chat_id, limit)?;
                Ok(ToolOutput::success(render_list(chat_id, &artefacts)))
            }
            "read" => {
                let id = input["id"]
                    .as_str()
                    .ok_or_else(|| DysonError::tool("artefacts", "missing or invalid 'id'"))?;
                if !safe_store_id(id) {
                    return Ok(ToolOutput::error("invalid artefact id"));
                }
                let Some(record) = reader.read(chat_id, id)? else {
                    return Ok(ToolOutput::error(format!(
                        "artefact '{id}' was not found in the current chat"
                    )));
                };
                let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
                let limit = input["limit"].as_u64().map(|n| n as usize);
                Ok(ToolOutput::success(render_record(&record, offset, limit)))
            }
            other => Ok(ToolOutput::error(format!(
                "unknown artefacts operation '{other}'"
            ))),
        }
    }
}

fn bounded_limit(value: Option<u64>, default: usize, max: usize) -> usize {
    value
        .map(|v| v.max(1).min(max as u64) as usize)
        .unwrap_or(default)
}

pub fn safe_store_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn render_list(chat_id: &str, artefacts: &[ArtefactSummary]) -> String {
    if artefacts.is_empty() {
        return format!("No artefacts found for chat {chat_id}.");
    }

    let mut out = format!("Artefacts for chat {chat_id}:\n");
    for artefact in artefacts {
        let tool = artefact
            .tool_use_id
            .as_deref()
            .map(|id| format!(", tool_use_id: {id}"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "- {} [{}] {} ({} bytes, created_at: {}{})",
            artefact.id, artefact.kind, artefact.title, artefact.bytes, artefact.created_at, tool
        );
    }
    out
}

fn render_record(record: &ArtefactRecord, offset: usize, limit: Option<usize>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# {}", record.title);
    let _ = writeln!(out);
    let _ = writeln!(out, "Artefact: {}", record.id);
    let _ = writeln!(out, "Chat: {}", record.chat_id);
    let _ = writeln!(out, "Kind: {}", record.kind);
    let _ = writeln!(out, "MIME: {}", record.mime_type);
    let _ = writeln!(out, "Bytes: {}", record.bytes);
    let _ = writeln!(out, "Created At: {}", record.created_at);
    if let Some(tool_use_id) = record.tool_use_id.as_deref() {
        let _ = writeln!(out, "Tool Use Id: {tool_use_id}");
    }
    if let Some(metadata) = record.metadata.as_ref() {
        let _ = writeln!(out, "Metadata: {metadata}");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "---");

    let (body, note) = slice_content(&record.content, offset, limit);
    if let Some(note) = note {
        let _ = writeln!(out, "{note}");
    }
    out.push_str(&body);
    truncate_output(&out).into_owned()
}

fn slice_content(content: &str, offset: usize, limit: Option<usize>) -> (String, Option<String>) {
    let start = offset.saturating_sub(1);
    let max = limit.unwrap_or(usize::MAX);
    let total = content.lines().count();
    let mut selected = String::new();
    for line in content.lines().skip(start).take(max) {
        selected.push_str(line);
        selected.push('\n');
    }
    if selected.is_empty() && content.is_empty() {
        return ("(empty artefact)\n".to_string(), None);
    }
    let shown = selected.lines().count();
    let note = if offset > 1 || limit.is_some() || start + shown < total {
        Some(format!(
            "[Showing lines {}-{} of {}]",
            offset,
            start + shown,
            total
        ))
    } else {
        None
    };
    (selected, note)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeReader {
        items: Vec<ArtefactRecord>,
    }

    impl ArtefactReader for FakeReader {
        fn list(&self, chat_id: &str, limit: usize) -> Result<Vec<ArtefactSummary>> {
            Ok(self
                .items
                .iter()
                .filter(|item| item.chat_id == chat_id)
                .take(limit)
                .map(|item| ArtefactSummary {
                    id: item.id.clone(),
                    chat_id: item.chat_id.clone(),
                    kind: item.kind.clone(),
                    title: item.title.clone(),
                    bytes: item.bytes,
                    created_at: item.created_at,
                    tool_use_id: item.tool_use_id.clone(),
                    metadata: item.metadata.clone(),
                })
                .collect())
        }

        fn read(&self, chat_id: &str, id: &str) -> Result<Option<ArtefactRecord>> {
            Ok(self
                .items
                .iter()
                .find(|item| item.chat_id == chat_id && item.id == id)
                .cloned())
        }
    }

    fn record(chat_id: &str, id: &str, title: &str) -> ArtefactRecord {
        let content = "one\ntwo\nthree\n".to_string();
        ArtefactRecord {
            id: id.to_string(),
            chat_id: chat_id.to_string(),
            kind: "security_review".to_string(),
            title: title.to_string(),
            bytes: content.len(),
            content,
            mime_type: "text/markdown".to_string(),
            created_at: 42,
            tool_use_id: Some("tool_1".to_string()),
            metadata: None,
        }
    }

    fn ctx() -> ToolContext {
        let mut ctx = ToolContext::from_cwd().unwrap();
        ctx.current_chat_id = Some("c1".to_string());
        ctx.artefacts = Some(std::sync::Arc::new(FakeReader {
            items: vec![
                record("c1", "a1", "Current chat report"),
                record("c2", "a1", "Other chat report"),
            ],
        }));
        ctx
    }

    #[tokio::test]
    async fn lists_only_current_chat_artefacts() {
        let out = ArtefactsTool
            .run(&json!({"operation": "list"}), &ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("Current chat report"));
        assert!(!out.content.contains("Other chat report"));
    }

    #[tokio::test]
    async fn reads_current_chat_artefact_by_id() {
        let out = ArtefactsTool
            .run(
                &json!({"operation": "read", "id": "a1", "offset": 2, "limit": 1}),
                &ctx(),
            )
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("# Current chat report"));
        assert!(out.content.contains("[Showing lines 2-2 of 3]"));
        assert!(out.content.contains("two"));
        assert!(!out.content.contains("one"));
    }

    #[tokio::test]
    async fn rejects_unsafe_ids_before_reader_lookup() {
        let out = ArtefactsTool
            .run(&json!({"operation": "read", "id": "../a1"}), &ctx())
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(out.content.contains("invalid artefact id"));
    }
}
