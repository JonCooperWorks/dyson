// ===========================================================================
// NotificationRouter — inbound (server -> client) MCP message dispatch.
//
// The transports' background readers classify every server-originated
// frame and hand requests/notifications to an `InboundHandler`
// (transport.rs).  This router is the `InboundHandler` the McpSkill
// installs once per connection.  It owns:
//
//   * notification dispatch: `notifications/message` (logging),
//     `notifications/progress`, `notifications/cancelled`, and the
//     `*/list_changed` family.
//   * server-originated *requests* (sampling/createMessage, roots/list,
//     elicitation/create).  Those need dyson internals (the LlmClient,
//     the workspace, the controller's UI channel) that are wired in as
//     they land; until then we answer `-32601 Method not found`, which
//     is correct because the client advertised no such capability.
//
// Logging notifications are routed through `tracing` with structured
// fields so they surface in dyson's normal logs and (later) an
// `SseEvent::McpLog` for the web UI.
// ===========================================================================

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;

use crate::config::AgentSettings;
use crate::workspace::WorkspaceHandle;

use super::protocol::JsonRpcError;
use super::transport::InboundHandler;

/// Marker that the connection had enough local context to support
/// server-originated sampling, if a future trusted opt-in enables it.
pub struct SamplingBackend;

impl SamplingBackend {
    pub fn new(_settings: AgentSettings, _workspace: Option<WorkspaceHandle>) -> Self {
        Self
    }
}

/// Routes server-originated MCP traffic for a single connection.
pub struct NotificationRouter {
    /// The MCP server this router belongs to — included in every log
    /// line so multi-server deployments stay legible.
    server_name: String,
    /// Filesystem roots advertised to the server via `roots/list`.  These
    /// are the directories the client is willing to let the server reason
    /// about (the agent's working directory).  Empty means we advertise
    /// no `roots` capability and answer `roots/list` with an empty list.
    roots: Vec<PathBuf>,
    /// Backs `sampling/createMessage`.  `None` ⇒ we advertise no
    /// `sampling` capability and answer the request with -32601.
    sampling: Option<SamplingBackend>,
}

impl NotificationRouter {
    pub fn new(
        server_name: impl Into<String>,
        roots: Vec<PathBuf>,
        sampling: Option<SamplingBackend>,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            roots,
            sampling,
        }
    }

    /// Build the `roots/list` result from the configured root paths.
    fn roots_result(&self) -> Value {
        let roots: Vec<Value> = self
            .roots
            .iter()
            .map(|p| {
                let name = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("root")
                    .to_string();
                serde_json::json!({
                    "uri": format!("file://{}", p.display()),
                    "name": name
                })
            })
            .collect();
        serde_json::json!({ "roots": roots })
    }

    /// Route a `notifications/message` (MCP logging) payload through
    /// `tracing` at the mapped level.  The MCP `level` follows syslog
    /// severities; we collapse them onto tracing's five levels.  `logger`
    /// and `data` are spec-optional.
    fn route_log(&self, params: Option<Value>) {
        let params = params.unwrap_or(Value::Null);
        let level = params
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("info");
        let logger = params.get("logger").and_then(Value::as_str);
        // `data` is arbitrary JSON; render compactly for the log line.
        let data = params
            .get("data")
            .map(|d| d.to_string())
            .unwrap_or_default();
        match level {
            // syslog: emergency/alert/critical/error -> ERROR
            "emergency" | "alert" | "critical" | "error" => tracing::error!(
                server = %self.server_name, logger, mcp_level = level, "{data}"
            ),
            "warning" => tracing::warn!(
                server = %self.server_name, logger, mcp_level = level, "{data}"
            ),
            "notice" | "info" => tracing::info!(
                server = %self.server_name, logger, mcp_level = level, "{data}"
            ),
            // debug and anything unrecognized
            _ => tracing::debug!(
                server = %self.server_name, logger, mcp_level = level, "{data}"
            ),
        }
    }
}

#[async_trait]
impl InboundHandler for NotificationRouter {
    async fn handle_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> std::result::Result<Value, JsonRpcError> {
        match method {
            // The server is asking which filesystem roots we expose.  We
            // only advertise (and answer) this when we actually have roots.
            "roots/list" if !self.roots.is_empty() => Ok(self.roots_result()),
            // The server wants us to run an LLM completion on its behalf.
            "sampling/createMessage" => {
                if self.sampling.is_some() {
                    tracing::warn!(
                        server = %self.server_name,
                        "refusing server-originated MCP sampling; no trusted opt-in is configured"
                    );
                }
                Err(method_not_found(method))
            }
            // The server wants the user to fill in a small form.  Park it
            // for the web UI to answer; only honored when a UI is present.
            "elicitation/create" if super::elicitation::ui_enabled() => {
                let params = params.unwrap_or(Value::Null);
                let message = params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let schema = params
                    .get("requestedSchema")
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(super::elicitation::broker()
                    .elicit(self.server_name.clone(), message, schema)
                    .await)
            }
            // Any other method (or sampling/elicitation without a backend
            // or UI) gets the spec-correct "method not found".
            _ => Err(method_not_found(method)),
        }
    }

    async fn handle_notification(&self, method: &str, params: Option<Value>) {
        match method {
            "notifications/message" => self.route_log(params),
            "notifications/progress" => {
                let token = params
                    .as_ref()
                    .and_then(|p| p.get("progressToken"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let progress = params
                    .as_ref()
                    .and_then(|p| p.get("progress"))
                    .cloned()
                    .unwrap_or(Value::Null);
                tracing::debug!(
                    server = %self.server_name,
                    token = %token,
                    progress = %progress,
                    "MCP progress notification"
                );
            }
            "notifications/cancelled" => {
                tracing::debug!(
                    server = %self.server_name,
                    params = ?params,
                    "MCP cancellation notification"
                );
            }
            "notifications/tools/list_changed"
            | "notifications/resources/list_changed"
            | "notifications/resources/updated"
            | "notifications/prompts/list_changed" => {
                // NOT YET IMPLEMENTED: we don't invalidate or re-list the
                // peer's catalogue here, so a stale catalogue may be served
                // until the next natural refresh.  Record the change so the
                // signal is observable in logs; wire up invalidation +
                // re-listing via the subscription registry when that lands.
                tracing::info!(
                    server = %self.server_name,
                    method,
                    "MCP list-changed notification"
                );
            }
            other => {
                tracing::debug!(
                    server = %self.server_name,
                    method = other,
                    "unhandled MCP notification"
                );
            }
        }
    }
}

fn method_not_found(method: &str) -> JsonRpcError {
    JsonRpcError {
        code: -32601,
        message: format!("Method not found: {method}"),
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_inbound_request_is_method_not_found() {
        let router = NotificationRouter::new("srv", vec![], None);
        let err = router
            .handle_request("sampling/createMessage", None)
            .await
            .expect_err("no capability advertised yet");
        assert_eq!(err.code, -32601);
    }

    #[tokio::test]
    async fn roots_list_returns_configured_roots() {
        let router = NotificationRouter::new("srv", vec![PathBuf::from("/work/agent")], None);
        let result = router.handle_request("roots/list", None).await.unwrap();
        assert_eq!(result["roots"][0]["uri"], "file:///work/agent");
        assert_eq!(result["roots"][0]["name"], "agent");
    }

    #[tokio::test]
    async fn elicitation_without_ui_is_method_not_found() {
        // enable_ui() is only called by the HTTP controller at startup, so
        // in lib tests the UI is off and elicitation/create is refused.
        let router = NotificationRouter::new("srv", vec![], None);
        let err = router
            .handle_request(
                "elicitation/create",
                Some(serde_json::json!({ "message": "hi" })),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, -32601);
    }

    #[tokio::test]
    async fn sampling_without_backend_is_method_not_found() {
        let router = NotificationRouter::new("srv", vec![], None);
        let err = router
            .handle_request("sampling/createMessage", None)
            .await
            .unwrap_err();
        assert_eq!(err.code, -32601);
    }

    #[tokio::test]
    async fn sampling_with_backend_is_still_refused_by_default() {
        let router = NotificationRouter::new(
            "srv",
            vec![],
            Some(SamplingBackend::new(AgentSettings::default(), None)),
        );
        let err = router
            .handle_request("sampling/createMessage", None)
            .await
            .unwrap_err();
        assert_eq!(
            err.code, -32601,
            "server-originated sampling must not be enabled without an explicit trusted opt-in"
        );
    }

    #[tokio::test]
    async fn roots_list_is_method_not_found_when_no_roots() {
        // With no roots we advertise no `roots` capability, so a server
        // that asks anyway gets the spec-correct refusal.
        let router = NotificationRouter::new("srv", vec![], None);
        let err = router.handle_request("roots/list", None).await.unwrap_err();
        assert_eq!(err.code, -32601);
    }

    #[tokio::test]
    async fn logging_notification_is_accepted_at_every_level() {
        let router = NotificationRouter::new("srv", vec![], None);
        for level in [
            "debug", "info", "notice", "warning", "error", "critical", "weird",
        ] {
            // Should not panic regardless of level (including unknown).
            router
                .handle_notification(
                    "notifications/message",
                    Some(serde_json::json!({ "level": level, "data": "hello" })),
                )
                .await;
        }
    }

    #[tokio::test]
    async fn progress_and_list_changed_are_handled() {
        let router = NotificationRouter::new("srv", vec![], None);
        router
            .handle_notification(
                "notifications/progress",
                Some(serde_json::json!({ "progressToken": "t1", "progress": 0.5 })),
            )
            .await;
        router
            .handle_notification("notifications/tools/list_changed", None)
            .await;
    }
}
