//! Prompt template listing and expansion.
use super::protocol;
use super::text::sanitize_mcp_description;
use super::transport::McpTransport;
use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use std::sync::Arc;

/// Tool exposing an MCP server's `prompts/list` + `prompts/get` surface.
/// Registered only when the server advertised the `prompts` capability.
pub(super) struct McpPromptsTool {
    pub(super) tool_name: String,
    pub(super) transport: Arc<dyn McpTransport>,
    pub(super) server_name: String,
}

#[async_trait]
impl Tool for McpPromptsTool {
    fn name(&self) -> &str {
        &self.tool_name
    }
    fn description(&self) -> &str {
        "List and expand prompt templates exposed by this MCP server. \
         Use op=\"list\" to discover prompt names and their arguments, then \
         op=\"get\" with a \"name\" (and optional \"arguments\" object) to \
         expand a template into its messages."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "op": { "type": "string", "enum": ["list", "get"] },
                "name": { "type": "string", "description": "Prompt name (required for op=get)" },
                "arguments": { "type": "object", "description": "Template arguments for op=get" }
            },
            "required": ["op"]
        })
    }

    async fn run(&self, input: &serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        match input["op"].as_str() {
            Some("list") => self.list().await,
            Some("get") => match input["name"].as_str() {
                Some(name) => self.get(name, input.get("arguments").cloned()).await,
                None => Ok(ToolOutput::error("op=get requires a \"name\"")),
            },
            _ => Ok(ToolOutput::error("op must be \"list\" or \"get\"")),
        }
    }
}

impl McpPromptsTool {
    async fn list(&self) -> Result<ToolOutput> {
        let result_json = self
            .transport
            .send_request("prompts/list", Some(serde_json::json!({})))
            .await
            .map_err(|e| DysonError::Mcp {
                server: self.server_name.clone(),
                message: format!("prompts/list failed: {e}"),
            })?;
        let list: protocol::McpPromptsListResult =
            serde_json::from_value(result_json).map_err(|e| DysonError::Mcp {
                server: self.server_name.clone(),
                message: format!("failed to parse prompts/list: {e}"),
            })?;
        if list.prompts.is_empty() {
            return Ok(ToolOutput::success("No prompts exposed by this server."));
        }
        let mut lines = Vec::with_capacity(list.prompts.len());
        for p in &list.prompts {
            // Prompt metadata is server-controlled — sanitize free text.
            let desc = p
                .description
                .as_deref()
                .map(sanitize_mcp_description)
                .filter(|d| !d.is_empty());
            let args = if p.arguments.is_empty() {
                String::new()
            } else {
                let parts: Vec<String> = p
                    .arguments
                    .iter()
                    .map(|a| {
                        if a.required {
                            format!("{}*", a.name)
                        } else {
                            a.name.clone()
                        }
                    })
                    .collect();
                format!("  args: {}", parts.join(", "))
            };
            lines.push(format!(
                "- {name}{desc}{args}",
                name = p.name,
                desc = match desc {
                    Some(d) => format!("  — {d}"),
                    None => String::new(),
                },
            ));
        }
        Ok(ToolOutput::success(format!(
            "Prompts exposed by '{}' (* = required arg):\n{}",
            self.server_name,
            lines.join("\n")
        )))
    }

    async fn get(&self, name: &str, arguments: Option<serde_json::Value>) -> Result<ToolOutput> {
        let mut params = serde_json::json!({ "name": name });
        if let Some(args) = arguments {
            params["arguments"] = args;
        }
        let result_json = self
            .transport
            .send_request("prompts/get", Some(params))
            .await
            .map_err(|e| DysonError::Mcp {
                server: self.server_name.clone(),
                message: format!("prompts/get failed for '{name}': {e}"),
            })?;
        let got: protocol::McpPromptGetResult =
            serde_json::from_value(result_json).map_err(|e| DysonError::Mcp {
                server: self.server_name.clone(),
                message: format!("failed to parse prompts/get: {e}"),
            })?;
        let mut out = String::new();
        if let Some(desc) = got.description.as_deref().map(sanitize_mcp_description)
            && !desc.is_empty()
        {
            out.push_str(&desc);
            out.push_str("\n\n");
        }
        for msg in &got.messages {
            // A prompt message carries a single content block.  Render
            // text inline; mark non-text blocks rather than dumping them.
            let rendered = match msg.content.get("type").and_then(|t| t.as_str()) {
                Some("text") => msg
                    .content
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string(),
                Some(other) => format!("[{other} content block]"),
                None => "[empty content block]".to_string(),
            };
            out.push_str(&format!("[{}] {}\n", msg.role, rendered));
        }
        Ok(ToolOutput::success(out.trim_end().to_string()))
    }
}
