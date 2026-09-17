//! Remote tool calls and asynchronous task lifecycle.
use super::artifacts::{save_mcp_image, save_mcp_resource};
use super::protocol::{McpContent, McpToolResult};
use super::transport::McpTransport;
use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

pub(super) struct McpRemoteTool {
    pub(super) tool_name: String,
    pub(super) description: String,
    pub(super) input_schema: serde_json::Value,
    pub(super) transport: Arc<dyn McpTransport>,
    pub(super) server_name: String,
    /// When true, the server marked this tool `taskSupport: "required"` —
    /// invoke it via the task lifecycle (tools/call+task → tasks/get →
    /// tasks/result) instead of awaiting the result inline.
    pub(super) task_required: bool,
}

/// Default TTL we request for a task, and the hard ceiling on how long we
/// poll before giving up.  The server clamps the TTL to its own bound.
pub(super) const MCP_TASK_TTL_MS: u64 = 300_000;
pub(super) const MCP_TASK_MAX_WAIT: Duration = Duration::from_secs(300);

#[async_trait]
impl Tool for McpRemoteTool {
    fn name(&self) -> &str {
        &self.tool_name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if self.task_required {
            return self.run_as_task(input, ctx).await;
        }
        let params = serde_json::json!({ "name": self.tool_name, "arguments": input });
        let result_json = self
            .transport
            .send_request("tools/call", Some(params))
            .await
            .map_err(|e| DysonError::Mcp {
                server: self.server_name.clone(),
                message: format!("tools/call failed for '{}': {e}", self.tool_name),
            })?;
        let tool_result: McpToolResult =
            serde_json::from_value(result_json).map_err(|e| DysonError::Mcp {
                server: self.server_name.clone(),
                message: format!("failed to parse tools/call result: {e}"),
            })?;
        self.decode_tool_result(&tool_result)
    }
}

impl McpRemoteTool {
    /// Turn a parsed `CallToolResult` into a `ToolOutput`, saving image /
    /// resource blocks as files and emitting compact inline markers.
    /// Shared by the inline `tools/call` path and the task path.
    fn decode_tool_result(&self, tool_result: &McpToolResult) -> Result<ToolOutput> {
        let mut content_parts = Vec::new();
        let mut files = Vec::new();
        for (idx, block) in tool_result.content.iter().enumerate() {
            match block {
                McpContent::Text { text } => content_parts.push(text.clone()),
                McpContent::Image { data, mime_type } => {
                    let (path, bytes) =
                        save_mcp_image(&self.server_name, &self.tool_name, idx, mime_type, data)?;
                    files.push(path);
                    content_parts.push(format!("[image: {mime_type}, {bytes} bytes]"));
                }
                McpContent::Resource { resource } => {
                    let (path, bytes, original_name) =
                        save_mcp_resource(&self.server_name, &self.tool_name, idx, resource)?;
                    files.push(path);
                    // Mirrors the `[image: MIME, N bytes]` marker shape
                    // used by the Image variant — short, predictable,
                    // and the bytes themselves live in ToolOutput.files
                    // for the controller to deliver as an artefact.
                    content_parts.push(format!(
                        "[resource: {original_name}, {mime}, {bytes} bytes]",
                        mime = resource.mime_type,
                    ));
                }
                McpContent::Unknown => {
                    content_parts.push("[unsupported MCP content block]".to_string());
                }
            }
        }
        Ok(ToolOutput {
            content: content_parts.join("\n"),
            is_error: tool_result.is_error,
            view: None,
            metadata: Some(serde_json::json!({
                "dyson_output_kind": "mcp",
                "mcp_server": self.server_name,
                "mcp_tool": self.tool_name,
            })),
            files,
            checkpoints: vec![],
            artefacts: vec![],
        })
    }

    /// Run a `taskSupport: required` tool through the MCP task lifecycle:
    /// `tools/call` with a `task` augmentation returns a task handle; we
    /// poll `tasks/get` until the task leaves `working`/`input_required`,
    /// then fetch the real result with `tasks/result`.  Server-originated
    /// requests the task raises mid-flight (elicitation/sampling) are
    /// delivered to the NotificationRouter by the transport, so an
    /// `input_required` status resolves once the UI answers; we keep
    /// polling through it.  Honors `ctx.cancellation` (issues `tasks/cancel`).
    async fn run_as_task(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let mcp_err = |e: String| DysonError::Mcp {
            server: self.server_name.clone(),
            message: e,
        };

        let create = self
            .transport
            .send_request(
                "tools/call",
                Some(serde_json::json!({
                    "name": self.tool_name,
                    "arguments": input,
                    "task": { "ttl": MCP_TASK_TTL_MS },
                })),
            )
            .await
            .map_err(|e| {
                mcp_err(format!(
                    "task tools/call failed for '{}': {e}",
                    self.tool_name
                ))
            })?;

        let task = create.get("task").ok_or_else(|| {
            mcp_err(format!(
                "task-augmented call for '{}' returned no task handle",
                self.tool_name
            ))
        })?;
        let task_id = task
            .get("taskId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| mcp_err("task handle missing taskId".into()))?
            .to_string();
        let mut poll_ms = task
            .get("pollInterval")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1000)
            .clamp(100, 5000);
        let mut status = task
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("working")
            .to_string();

        let deadline = tokio::time::Instant::now() + MCP_TASK_MAX_WAIT;
        while matches!(status.as_str(), "working" | "input_required") {
            tokio::select! {
                () = ctx.cancellation.cancelled() => {
                    let _ = self
                        .transport
                        .send_request("tasks/cancel", Some(serde_json::json!({ "taskId": task_id })))
                        .await;
                    return Ok(ToolOutput::error(format!(
                        "MCP task for '{}' cancelled",
                        self.tool_name
                    )));
                }
                () = tokio::time::sleep(Duration::from_millis(poll_ms)) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = self
                    .transport
                    .send_request(
                        "tasks/cancel",
                        Some(serde_json::json!({ "taskId": task_id })),
                    )
                    .await;
                return Ok(ToolOutput::error(format!(
                    "MCP task for '{}' did not complete within {}s",
                    self.tool_name,
                    MCP_TASK_MAX_WAIT.as_secs()
                )));
            }
            let got = self
                .transport
                .send_request("tasks/get", Some(serde_json::json!({ "taskId": task_id })))
                .await
                .map_err(|e| mcp_err(format!("tasks/get failed: {e}")))?;
            status = got
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("working")
                .to_string();
            if let Some(p) = got.get("pollInterval").and_then(serde_json::Value::as_u64) {
                poll_ms = p.clamp(100, 5000);
            }
        }

        if status != "completed" {
            return Ok(ToolOutput::error(format!(
                "MCP task for '{}' ended with status '{status}'",
                self.tool_name
            )));
        }

        let result_json = self
            .transport
            .send_request(
                "tasks/result",
                Some(serde_json::json!({ "taskId": task_id })),
            )
            .await
            .map_err(|e| mcp_err(format!("tasks/result failed: {e}")))?;
        let tool_result: McpToolResult = serde_json::from_value(result_json)
            .map_err(|e| mcp_err(format!("failed to parse tasks/result: {e}")))?;
        self.decode_tool_result(&tool_result)
    }
}
