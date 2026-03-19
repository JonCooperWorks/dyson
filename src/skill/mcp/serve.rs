// ===========================================================================
// MCP HTTP server — expose Dyson's workspace tools to Claude Code via MCP.
//
// This is the *server* side of MCP (Dyson serves tools TO Claude Code).
// The client side (Dyson connecting to external MCP servers) lives in
// mod.rs, transport.rs, and protocol.rs.
//
// Architecture:
//   Dyson process
//     ├── McpHttpServer (tokio task, 127.0.0.1:random_port)
//     │     POST /mcp → JSON-RPC 2.0
//     │     methods: initialize, notifications/initialized, tools/list, tools/call
//     │     tools: workspace_view, workspace_search, workspace_update
//     │
//     └── ClaudeCodeClient
//           spawns: claude -p --mcp-config /tmp/dyson-mcp-XXX.json
//           Claude Code connects back to our HTTP server ↑
//
// Sandboxing:
//   The `dangerous_no_sandbox` flag is plumbed through for future use.
//   Today, workspace tools are in-memory operations that don't need
//   sandboxing. The hook is here so we can add sandbox enforcement
//   later without changing the API.
// ===========================================================================

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::tool::workspace_search::WorkspaceSearchTool;
use crate::tool::workspace_update::WorkspaceUpdateTool;
use crate::tool::workspace_view::WorkspaceViewTool;
use crate::tool::{Tool, ToolContext};
use crate::workspace::Workspace;

use super::protocol::{JsonRpcError, JsonRpcResponse, McpToolDef};

// ---------------------------------------------------------------------------
// McpHttpServer
// ---------------------------------------------------------------------------

/// An in-process HTTP MCP server that exposes workspace tools.
///
/// Binds to `127.0.0.1:0` (OS-assigned port) and handles JSON-RPC 2.0
/// requests from Claude Code over a single POST `/mcp` endpoint.
pub struct McpHttpServer {
    workspace: Arc<RwLock<Box<dyn Workspace>>>,
    tools: HashMap<String, Arc<dyn Tool>>,

    /// Plumbed through for future sandbox enforcement on tool calls.
    /// When false, a sandbox could gate tool execution. Today it's unused
    /// because workspace tools are pure in-memory operations.
    #[allow(dead_code)]
    dangerous_no_sandbox: bool,
}

impl McpHttpServer {
    /// Create a new MCP server exposing workspace tools.
    ///
    /// `dangerous_no_sandbox` is stored for future use — the hook is here
    /// so sandbox enforcement can be added later without API changes.
    pub fn new(
        workspace: Arc<RwLock<Box<dyn Workspace>>>,
        dangerous_no_sandbox: bool,
    ) -> Self {
        let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

        let view = Arc::new(WorkspaceViewTool) as Arc<dyn Tool>;
        let search = Arc::new(WorkspaceSearchTool) as Arc<dyn Tool>;
        let update = Arc::new(WorkspaceUpdateTool) as Arc<dyn Tool>;

        tools.insert(view.name().to_string(), view);
        tools.insert(search.name().to_string(), search);
        tools.insert(update.name().to_string(), update);

        Self {
            workspace,
            tools,
            dangerous_no_sandbox,
        }
    }

    /// Start the HTTP server on a random loopback port.
    ///
    /// Returns `(port, task_handle)`. The server runs until the task is
    /// aborted or the cancellation token fires.
    pub async fn start(self: Arc<Self>) -> Result<(u16, JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();

        tracing::info!(port = port, "MCP HTTP server listening");

        let server = Arc::clone(&self);
        let handle = tokio::spawn(async move {
            loop {
                let (stream, _addr) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::warn!(error = %e, "MCP server accept error");
                        continue;
                    }
                };

                let server = Arc::clone(&server);
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        let server = Arc::clone(&server);
                        async move { Ok::<_, Infallible>(server.handle_request(req).await) }
                    });

                    if let Err(e) = http1::Builder::new()
                        .serve_connection(io, service)
                        .await
                    {
                        tracing::debug!(error = %e, "MCP HTTP connection error");
                    }
                });
            }
        });

        Ok((port, handle))
    }

    /// Route an HTTP request.
    ///
    /// Only POST /mcp is handled — everything else gets 404.
    async fn handle_request(
        &self,
        req: Request<hyper::body::Incoming>,
    ) -> Response<Full<Bytes>> {
        if req.method() != hyper::Method::POST || req.uri().path() != "/mcp" {
            return json_response(StatusCode::NOT_FOUND, &serde_json::json!({
                "error": "not found"
            }));
        }

        // Read the full body.
        let body = match req.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(e) => {
                tracing::warn!(error = %e, "MCP server: failed to read request body");
                return json_response(StatusCode::BAD_REQUEST, &serde_json::json!({
                    "error": "failed to read body"
                }));
            }
        };

        // Parse JSON-RPC request.
        let json: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "MCP server: invalid JSON");
                return json_response(StatusCode::BAD_REQUEST, &JsonRpcResponse {
                    id: None,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: "Parse error".into(),
                        data: None,
                    }),
                });
            }
        };

        let id = json.get("id").and_then(|v| v.as_u64());
        let method = json["method"].as_str().unwrap_or("");
        let params = json.get("params").cloned();

        tracing::debug!(method = method, id = ?id, "MCP server: handling request");

        let response = self.dispatch(id, method, params).await;
        json_response(StatusCode::OK, &response)
    }

    /// Dispatch a JSON-RPC method to the appropriate handler.
    async fn dispatch(
        &self,
        id: Option<u64>,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> JsonRpcResponse {
        match method {
            "initialize" => self.handle_initialize(id),
            "notifications/initialized" => self.handle_notification(id),
            "tools/list" => self.handle_tools_list(id),
            "tools/call" => self.handle_tools_call(id, params).await,
            _ => JsonRpcResponse {
                id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("Method not found: {method}"),
                    data: None,
                }),
            },
        }
    }

    /// Handle `initialize` — return server capabilities.
    fn handle_initialize(&self, id: Option<u64>) -> JsonRpcResponse {
        JsonRpcResponse {
            id,
            result: Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "dyson-workspace",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })),
            error: None,
        }
    }

    /// Handle `notifications/initialized` — acknowledge, no result needed.
    fn handle_notification(&self, id: Option<u64>) -> JsonRpcResponse {
        JsonRpcResponse {
            id,
            result: Some(serde_json::json!({})),
            error: None,
        }
    }

    /// Handle `tools/list` — return workspace tool definitions.
    fn handle_tools_list(&self, id: Option<u64>) -> JsonRpcResponse {
        let tool_defs: Vec<McpToolDef> = self
            .tools
            .values()
            .map(|tool| McpToolDef {
                name: tool.name().to_string(),
                description: Some(tool.description().to_string()),
                input_schema: Some(tool.input_schema()),
            })
            .collect();

        JsonRpcResponse {
            id,
            result: Some(serde_json::json!({ "tools": tool_defs })),
            error: None,
        }
    }

    /// Handle `tools/call` — execute a workspace tool.
    async fn handle_tools_call(
        &self,
        id: Option<u64>,
        params: Option<serde_json::Value>,
    ) -> JsonRpcResponse {
        let params = match params {
            Some(p) => p,
            None => {
                return JsonRpcResponse {
                    id,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32602,
                        message: "Missing params".into(),
                        data: None,
                    }),
                };
            }
        };

        let tool_name = match params["name"].as_str() {
            Some(n) => n,
            None => {
                return JsonRpcResponse {
                    id,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32602,
                        message: "Missing tool name in params".into(),
                        data: None,
                    }),
                };
            }
        };

        let tool = match self.tools.get(tool_name) {
            Some(t) => Arc::clone(t),
            None => {
                return JsonRpcResponse {
                    id,
                    result: Some(serde_json::json!({
                        "content": [{ "type": "text", "text": format!("Unknown tool: {tool_name}") }],
                        "isError": true
                    })),
                    error: None,
                };
            }
        };

        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        // Build a ToolContext with the workspace reference.
        let ctx = ToolContext {
            working_dir: std::env::current_dir().unwrap_or_default(),
            env: HashMap::new(),
            cancellation: CancellationToken::new(),
            workspace: Some(Arc::clone(&self.workspace)),
        };

        // TODO: When dangerous_no_sandbox is false, a sandbox could gate
        // this call. For now workspace tools are in-memory and don't need it.

        match tool.run(arguments, &ctx).await {
            Ok(output) => JsonRpcResponse {
                id,
                result: Some(serde_json::json!({
                    "content": [{ "type": "text", "text": output.content }],
                    "isError": output.is_error
                })),
                error: None,
            },
            Err(e) => JsonRpcResponse {
                id,
                result: Some(serde_json::json!({
                    "content": [{ "type": "text", "text": format!("Tool error: {e}") }],
                    "isError": true
                })),
                error: None,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a JSON HTTP response.
fn json_response<T: serde::Serialize>(
    status: StatusCode,
    body: &T,
) -> Response<Full<Bytes>> {
    let json = serde_json::to_vec(body).unwrap_or_default();
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(json)))
        .unwrap()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal workspace implementation for testing.
    struct MockWorkspace {
        files: std::collections::HashMap<String, String>,
    }

    impl MockWorkspace {
        fn new() -> Self {
            let mut files = std::collections::HashMap::new();
            files.insert("identity".to_string(), "I am a test agent".to_string());
            Self { files }
        }
    }

    impl Workspace for MockWorkspace {
        fn get(&self, name: &str) -> Option<String> {
            self.files.get(name).cloned()
        }

        fn set(&mut self, name: &str, content: &str) {
            self.files.insert(name.to_string(), content.to_string());
        }

        fn append(&mut self, name: &str, content: &str) {
            self.files
                .entry(name.to_string())
                .or_default()
                .push_str(content);
        }

        fn save(&self) -> crate::error::Result<()> {
            Ok(())
        }

        fn list_files(&self) -> Vec<String> {
            self.files.keys().cloned().collect()
        }

        fn search(&self, pattern: &str) -> Vec<(String, Vec<String>)> {
            let pattern_lower = pattern.to_lowercase();
            self.files
                .iter()
                .filter_map(|(name, content)| {
                    let matches: Vec<String> = content
                        .lines()
                        .filter(|line| line.to_lowercase().contains(&pattern_lower))
                        .map(|s| s.to_string())
                        .collect();
                    if matches.is_empty() {
                        None
                    } else {
                        Some((name.clone(), matches))
                    }
                })
                .collect()
        }

        fn system_prompt(&self) -> String {
            "mock workspace".into()
        }

        fn journal(&mut self, entry: &str) {
            self.append("journal", entry);
        }
    }

    fn make_server() -> Arc<McpHttpServer> {
        let ws: Arc<RwLock<Box<dyn Workspace>>> =
            Arc::new(RwLock::new(Box::new(MockWorkspace::new())));
        Arc::new(McpHttpServer::new(ws, true))
    }

    #[tokio::test]
    async fn initialize_returns_capabilities() {
        let server = make_server();
        let resp = server.dispatch(Some(1), "initialize", None).await;
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn tools_list_returns_workspace_tools() {
        let server = make_server();
        let resp = server.dispatch(Some(2), "tools/list", None).await;
        assert!(resp.error.is_none());
        let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
        assert_eq!(tools.len(), 3);

        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"workspace_view"));
        assert!(names.contains(&"workspace_search"));
        assert!(names.contains(&"workspace_update"));
    }

    #[tokio::test]
    async fn tools_call_workspace_view() {
        let server = make_server();
        let params = serde_json::json!({
            "name": "workspace_view",
            "arguments": { "file": "identity" }
        });
        let resp = server
            .dispatch(Some(3), "tools/call", Some(params))
            .await;
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("test agent"));
    }

    #[tokio::test]
    async fn tools_call_unknown_tool() {
        let server = make_server();
        let params = serde_json::json!({
            "name": "nonexistent",
            "arguments": {}
        });
        let resp = server
            .dispatch(Some(4), "tools/call", Some(params))
            .await;
        assert!(resp.error.is_none()); // MCP returns tool error in result, not JSON-RPC error
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let server = make_server();
        let resp = server.dispatch(Some(5), "bogus/method", None).await;
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[tokio::test]
    async fn server_binds_and_accepts() {
        let server = make_server();
        let (port, handle) = server.start().await.unwrap();
        assert!(port > 0);

        // Send a real HTTP request to the server.
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/mcp"))
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0.0.1" }
                }
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["result"]["protocolVersion"], "2024-11-05");

        handle.abort();
    }
}
