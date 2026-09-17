//! Resource listing, reading, and artifact output.
use super::artifacts::{INLINE_TEXT_CAP, floor_char_boundary, save_mcp_resource};
use super::protocol;
use super::text::sanitize_mcp_description;
use super::transport::McpTransport;
use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use std::sync::Arc;

/// Tool exposing an MCP server's `resources/list` + `resources/read`
/// surface to the agent.  Registered only when the server advertised the
/// `resources` capability during the handshake.
pub(super) struct McpResourcesTool {
    pub(super) tool_name: String,
    pub(super) transport: Arc<dyn McpTransport>,
    pub(super) server_name: String,
}

#[async_trait]
impl Tool for McpResourcesTool {
    fn name(&self) -> &str {
        &self.tool_name
    }
    fn description(&self) -> &str {
        "List and read resources exposed by this MCP server. \
         Use op=\"list\" to discover resource URIs, then op=\"read\" with a \
         \"uri\" to fetch one. Text bodies are inlined in the tool output \
         (truncated if very large); binary bodies and the full body are \
         always also saved as a workspace artefact."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "op": { "type": "string", "enum": ["list", "read"] },
                "uri": { "type": "string", "description": "Resource URI (required for op=read)" }
            },
            "required": ["op"]
        })
    }

    async fn run(&self, input: &serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        match input["op"].as_str() {
            Some("list") => self.list().await,
            Some("read") => match input["uri"].as_str() {
                Some(uri) => self.read(uri).await,
                None => Ok(ToolOutput::error("op=read requires a \"uri\"")),
            },
            _ => Ok(ToolOutput::error("op must be \"list\" or \"read\"")),
        }
    }
}

impl McpResourcesTool {
    async fn list(&self) -> Result<ToolOutput> {
        let result_json = self
            .transport
            .send_request("resources/list", Some(serde_json::json!({})))
            .await
            .map_err(|e| DysonError::Mcp {
                server: self.server_name.clone(),
                message: format!("resources/list failed: {e}"),
            })?;
        let list: protocol::McpResourcesListResult =
            serde_json::from_value(result_json).map_err(|e| DysonError::Mcp {
                server: self.server_name.clone(),
                message: format!("failed to parse resources/list: {e}"),
            })?;
        if list.resources.is_empty() {
            return Ok(ToolOutput::success("No resources exposed by this server."));
        }
        let mut lines = Vec::with_capacity(list.resources.len());
        for r in &list.resources {
            // Resource metadata is server-controlled — sanitize the
            // free-text fields the same way we do tool descriptions.
            let name = r.name.as_deref().unwrap_or("");
            let desc = r.description.as_deref().map(sanitize_mcp_description);
            let mime = r.mime_type.as_deref().unwrap_or("");
            lines.push(format!(
                "- {uri}{name}{mime}{desc}",
                uri = r.uri,
                name = if name.is_empty() {
                    String::new()
                } else {
                    format!("  ({name})")
                },
                mime = if mime.is_empty() {
                    String::new()
                } else {
                    format!("  [{mime}]")
                },
                desc = match desc {
                    Some(d) if !d.is_empty() => format!("  — {d}"),
                    _ => String::new(),
                },
            ));
        }
        Ok(ToolOutput::success(format!(
            "Resources exposed by '{}':\n{}",
            self.server_name,
            lines.join("\n")
        )))
    }

    async fn read(&self, uri: &str) -> Result<ToolOutput> {
        let result_json = self
            .transport
            .send_request("resources/read", Some(serde_json::json!({ "uri": uri })))
            .await
            .map_err(|e| DysonError::Mcp {
                server: self.server_name.clone(),
                message: format!("resources/read failed for '{uri}': {e}"),
            })?;
        let read: protocol::McpResourcesReadResult =
            serde_json::from_value(result_json).map_err(|e| DysonError::Mcp {
                server: self.server_name.clone(),
                message: format!("failed to parse resources/read: {e}"),
            })?;
        if read.contents.is_empty() {
            return Ok(ToolOutput::error(format!(
                "Resource '{uri}' had no contents"
            )));
        }
        let mut content_parts = Vec::new();
        let mut files = Vec::new();
        for (idx, resource) in read.contents.iter().enumerate() {
            let (path, bytes, original_name) =
                save_mcp_resource(&self.server_name, &self.tool_name, idx, resource)?;
            files.push(path);
            content_parts.push(format!(
                "[resource: {original_name}, {mime}, {bytes} bytes]",
                mime = resource.mime_type,
            ));
            // Text bodies are inlined so the agent can read the resource
            // without having to open the artefact file.  Binary blobs stay
            // file-only — round-tripping base64 through the LLM wastes
            // tokens and almost never helps.
            if !resource.text.is_empty() {
                if resource.text.len() <= INLINE_TEXT_CAP {
                    content_parts.push(resource.text.clone());
                } else {
                    let head_end = floor_char_boundary(&resource.text, INLINE_TEXT_CAP);
                    content_parts.push(format!(
                        "{head}\n[…truncated at {INLINE_TEXT_CAP} bytes; full {bytes}-byte body in artefact]",
                        head = &resource.text[..head_end],
                    ));
                }
            }
        }
        Ok(ToolOutput {
            content: content_parts.join("\n"),
            is_error: false,
            view: None,
            metadata: Some(serde_json::json!({
                "dyson_output_kind": "mcp",
                "mcp_server": self.server_name,
                "mcp_resource_uri": uri,
            })),
            files,
            checkpoints: vec![],
            artefacts: vec![],
        })
    }
}
