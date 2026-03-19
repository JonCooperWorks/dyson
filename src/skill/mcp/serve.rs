// ===========================================================================
// MCP HTTP server — expose Dyson's workspace tools to Claude Code via MCP.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements the *server* side of the Model Context Protocol (MCP).
//   While mod.rs/transport.rs connect Dyson *to* external MCP servers
//   (Dyson as client), this file makes Dyson an MCP *server* — exposing
//   workspace tools to Claude Code so it can read/search/update the
//   agent's workspace files.
//
// Why does Dyson need to be an MCP server?
//   When using Claude Code as the LLM backend (`provider: "claude_code"`),
//   Claude Code runs its own agent loop — Dyson can't inject tools into
//   its loop directly.  But Claude Code *does* support connecting to MCP
//   servers via `--mcp-config`.  So we flip the relationship:
//
//     Normal Dyson:  Dyson agent loop → tool.run() → workspace
//     Claude Code:   Claude Code agent loop → MCP → Dyson HTTP server → workspace
//
//   This way, Claude Code can access workspace_view, workspace_search,
//   and workspace_update as first-class structured tools — with proper
//   JSON schemas, validation, and tool_use blocks — without Dyson needing
//   to duplicate Claude Code's built-in tool execution.
//
// Architecture:
//
//   ┌──────────────────────────────────────────────────────────────┐
//   │ Dyson process                                                │
//   │                                                              │
//   │  ┌─────────────────────┐     ┌──────────────────────────┐   │
//   │  │ ClaudeCodeClient    │     │ McpHttpServer            │   │
//   │  │                     │     │ (tokio task)             │   │
//   │  │ 1. Starts server ───┼────▶│ 127.0.0.1:{random_port} │   │
//   │  │ 2. Spawns claude -p │     │                          │   │
//   │  │    --mcp-config     │     │ POST /mcp                │   │
//   │  │    '{"mcpServers":  │     │   ├─ initialize          │   │
//   │  │     {"dyson-        │     │   ├─ notifications/      │   │
//   │  │      workspace":    │     │   │  initialized         │   │
//   │  │      {"type":"sse", │     │   ├─ tools/list          │   │
//   │  │       "url":        │     │   │  → workspace_view    │   │
//   │  │       "http://...   │     │   │  → workspace_search  │   │
//   │  │       /mcp"}}}'     │     │   │  → workspace_update  │   │
//   │  │                     │     │   └─ tools/call          │   │
//   │  └──────┬──────────────┘     │      → runs Tool impl   │   │
//   │         │ stdin/stdout        └─────────────┬────────────┘   │
//   │         ▼                                   │               │
//   │  ┌──────────────┐                 ┌─────────▼───────┐       │
//   │  │ claude -p    │───HTTP/MCP────▶│ Arc<RwLock<     │       │
//   │  │ subprocess   │                 │   Box<dyn       │       │
//   │  │              │◀───responses────│   Workspace>>>  │       │
//   │  └──────────────┘                 └─────────────────┘       │
//   └──────────────────────────────────────────────────────────────┘
//
// MCP handshake sequence (server perspective):
//
//   1. Claude Code connects to http://127.0.0.1:{port}/mcp
//   2. Sends POST with `initialize` → we respond with capabilities
//   3. Sends POST with `notifications/initialized` → we acknowledge
//   4. Sends POST with `tools/list` → we return workspace tool definitions
//   5. During its agent loop, sends `tools/call` → we execute the tool
//
// HTTP transport details:
//
//   - Binds to 127.0.0.1:0 (loopback only, OS-assigned port)
//   - Loopback-only means no network exposure — only local processes
//     can reach the server
//   - OS-assigned port (port 0) avoids conflicts with other services
//   - Single endpoint: POST /mcp — all JSON-RPC requests go here
//   - Each request is a complete JSON-RPC 2.0 message
//   - Responses are JSON-RPC 2.0 responses
//   - The server runs as a tokio task; aborting the JoinHandle stops it
//
// Tool execution flow:
//
//   Claude Code calls tools/call with:
//     {"method":"tools/call","params":{"name":"workspace_view","arguments":{"file":"SOUL.md"}}}
//
//   McpHttpServer:
//     1. Looks up "workspace_view" in its tools HashMap
//     2. Builds a ToolContext with the shared workspace Arc<RwLock<...>>
//     3. Calls tool.run(arguments, &ctx) — same Tool trait used everywhere
//     4. Wraps the ToolOutput in MCP content blocks:
//        {"content":[{"type":"text","text":"# Agent Soul\n..."}],"isError":false}
//     5. Returns as JSON-RPC response
//
//   The tools are the exact same WorkspaceViewTool, WorkspaceSearchTool,
//   and WorkspaceUpdateTool that Dyson uses internally — no duplication.
//
// Sandboxing:
//
//   The `dangerous_no_sandbox` flag is plumbed through from the CLI
//   (`--dangerous-no-sandbox`) to McpHttpServer for future use.
//   Today, workspace tools are pure in-memory operations (read/write
//   a HashMap behind an RwLock) that don't need sandboxing.  The hook
//   is here so that when we add tools that touch the filesystem or
//   execute commands, we can gate them through the sandbox system
//   without changing any APIs or call sites.
//
//   Flow: CLI flag → Settings.dangerous_no_sandbox → create_client()
//         → ClaudeCodeClient.dangerous_no_sandbox → McpHttpServer
//
// Lifecycle:
//
//   The server's lifetime is tied to the LLM stream:
//     1. ClaudeCodeClient::stream() creates the server + starts it
//     2. The JoinHandle is moved into the async_stream closure
//     3. When the stream is dropped (turn complete or cancelled),
//        the JoinHandle is dropped, which aborts the tokio task
//     4. The server stops, the port is freed
//
//   This means a new server is created per LLM turn.  This is fine:
//   binding a TCP socket is ~0.1ms, and each turn takes seconds.
//
// Error handling:
//
//   - JSON parse errors → JSON-RPC -32700 (Parse error)
//   - Unknown methods → JSON-RPC -32601 (Method not found)
//   - Missing params → JSON-RPC -32602 (Invalid params)
//   - Unknown tool name → MCP tool error (isError: true in result)
//   - Tool execution failure → MCP tool error (isError: true)
//   - Non-POST or wrong path → HTTP 404
//
//   Note: MCP distinguishes between JSON-RPC errors (protocol-level)
//   and tool errors (application-level).  An unknown tool returns a
//   successful JSON-RPC response with isError: true in the result,
//   not a JSON-RPC error.  This matches the MCP specification.
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
///
/// ## How it works
///
/// The server wraps Dyson's existing workspace tools (view, search, update)
/// as MCP tool definitions.  When Claude Code calls `tools/list`, it gets
/// back the tool names, descriptions, and JSON schemas.  When it calls
/// `tools/call`, the server delegates to the same `Tool::run()` method
/// that Dyson's own agent loop uses.
///
/// ## Shared state
///
/// The workspace is shared via `Arc<RwLock<Box<dyn Workspace>>>`:
/// - Multiple concurrent reads (view, search) proceed in parallel
/// - Writes (update) get exclusive access via the RwLock
/// - The same Arc is shared with Dyson's internal tools if any are
///   running concurrently (e.g., during skill on_load)
///
/// ## Thread safety
///
/// The server spawns a new tokio task per TCP connection, and a service_fn
/// closure per HTTP request.  All share the same `Arc<McpHttpServer>`,
/// which is `Send + Sync` because:
/// - `HashMap<String, Arc<dyn Tool>>` is immutable after construction
/// - `Arc<RwLock<Box<dyn Workspace>>>` is designed for concurrent access
/// - `dangerous_no_sandbox` is a plain `bool` (Copy, immutable)
pub struct McpHttpServer {
    /// The agent's workspace, shared with other parts of Dyson.
    ///
    /// Used to construct a `ToolContext` for each `tools/call` invocation.
    /// The RwLock ensures safe concurrent access: reads are parallel,
    /// writes are exclusive.
    workspace: Arc<RwLock<Box<dyn Workspace>>>,

    /// Workspace tools indexed by name for O(1) dispatch.
    ///
    /// Populated once in `new()` with WorkspaceViewTool, WorkspaceSearchTool,
    /// and WorkspaceUpdateTool.  Never modified after construction.
    tools: HashMap<String, Arc<dyn Tool>>,

    /// Whether sandbox enforcement is bypassed.
    ///
    /// Plumbed through from the CLI `--dangerous-no-sandbox` flag for
    /// future use.  When `false` (the default), a sandbox implementation
    /// could gate tool calls before execution.  Today this field is unused
    /// because workspace tools are pure in-memory operations that don't
    /// need sandboxing.
    ///
    /// The hook is here so that:
    /// 1. Adding sandbox enforcement later requires zero API changes
    /// 2. The flag flows consistently through the entire call chain:
    ///    CLI → Settings → create_client() → ClaudeCodeClient → McpHttpServer
    #[allow(dead_code)]
    dangerous_no_sandbox: bool,
}

impl McpHttpServer {
    /// Create a new MCP server exposing workspace tools.
    ///
    /// Instantiates the three workspace tools (view, search, update) and
    /// indexes them by name for O(1) lookup during `tools/call`.
    ///
    /// ## Parameters
    ///
    /// - `workspace`: Shared workspace reference.  The same Arc can be
    ///   (and typically is) shared with Dyson's own internal tool context.
    /// - `dangerous_no_sandbox`: Whether the `--dangerous-no-sandbox` CLI
    ///   flag was passed.  Stored for future sandbox enforcement.  Has no
    ///   effect today — workspace tools are in-memory operations.
    ///
    /// ## Example
    ///
    /// ```ignore
    /// let ws: Arc<RwLock<Box<dyn Workspace>>> = /* ... */;
    /// let server = Arc::new(McpHttpServer::new(ws, true));
    /// let (port, handle) = server.start().await?;
    /// ```
    pub fn new(
        workspace: Arc<RwLock<Box<dyn Workspace>>>,
        dangerous_no_sandbox: bool,
    ) -> Self {
        let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

        // Create the three workspace tools.  These are the same Tool impls
        // used by Dyson's own agent loop — we're just wrapping them in MCP.
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
    /// Binds to `127.0.0.1:0` (loopback-only, OS-assigned port) and spawns
    /// a tokio task that accepts connections in a loop.  Each connection
    /// gets its own spawned task for concurrent request handling.
    ///
    /// ## Returns
    ///
    /// `(port, task_handle)` where:
    /// - `port`: The OS-assigned port number.  Used to construct the MCP
    ///   config JSON passed to Claude Code via `--mcp-config`.
    /// - `task_handle`: A `JoinHandle<()>` that owns the accept loop.
    ///   Dropping or aborting this handle stops the server and frees the
    ///   port.  The caller (ClaudeCodeClient) moves this into the stream
    ///   closure so the server lives exactly as long as the LLM turn.
    ///
    /// ## Why loopback-only?
    ///
    /// Security.  The MCP server has no authentication — any process that
    /// can reach it can read/write the workspace.  Binding to 127.0.0.1
    /// ensures only local processes can connect.  This is sufficient
    /// because the only intended client is the `claude -p` subprocess
    /// running on the same machine.
    ///
    /// ## Why port 0?
    ///
    /// Avoids port conflicts.  The OS picks an available ephemeral port.
    /// Since a new server is created per LLM turn (and turns are
    /// serialized), there's never more than one server running.
    ///
    /// ## Connection handling
    ///
    /// Uses hyper's HTTP/1.1 server with one-connection-per-task.  Each
    /// connection uses `service_fn` to dispatch requests to
    /// `handle_request()`.  Claude Code typically sends one request per
    /// connection (MCP over HTTP uses request/response, not streaming).
    pub async fn start(self: Arc<Self>) -> Result<(u16, JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        assert!(
            addr.ip().is_loopback(),
            "MCP server must bind to loopback only — got {addr}"
        );
        let port = addr.port();

        tracing::info!(port = port, "MCP HTTP server listening");

        let server = Arc::clone(&self);
        let handle = tokio::spawn(async move {
            loop {
                // Accept a new TCP connection.
                let (stream, _addr) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::warn!(error = %e, "MCP server accept error");
                        continue;
                    }
                };

                // Spawn a task per connection for concurrency.
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

    /// Route an incoming HTTP request to the JSON-RPC dispatcher.
    ///
    /// ## Request validation
    ///
    /// Only `POST /mcp` is accepted.  Everything else gets a 404.
    /// This single-endpoint design matches the MCP HTTP transport spec:
    /// all JSON-RPC messages go to one URL, distinguished by the `method`
    /// field in the JSON body.
    ///
    /// ## Processing pipeline
    ///
    /// 1. Validate method + path → 404 if wrong
    /// 2. Read the full request body into memory
    /// 3. Parse as JSON → JSON-RPC -32700 if invalid
    /// 4. Extract `id`, `method`, and `params` fields
    /// 5. Dispatch to the appropriate handler
    /// 6. Serialize the response as JSON and return
    ///
    /// ## Error responses
    ///
    /// - Non-POST or wrong path → HTTP 404 with `{"error":"not found"}`
    /// - Body read failure → HTTP 400
    /// - Invalid JSON → HTTP 400 with JSON-RPC error code -32700
    /// - All other errors → HTTP 200 with JSON-RPC error in response body
    ///   (per JSON-RPC spec, transport errors use HTTP status codes, but
    ///   application errors are returned in the JSON-RPC response)
    async fn handle_request(
        &self,
        req: Request<hyper::body::Incoming>,
    ) -> Response<Full<Bytes>> {
        if req.method() != hyper::Method::POST || req.uri().path() != "/mcp" {
            return json_response(StatusCode::NOT_FOUND, &serde_json::json!({
                "error": "not found"
            }));
        }

        // Read the full request body.  MCP requests are small (tool
        // arguments are typically a few KB at most) so buffering the
        // entire body is fine.
        let body = match req.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(e) => {
                tracing::warn!(error = %e, "MCP server: failed to read request body");
                return json_response(StatusCode::BAD_REQUEST, &serde_json::json!({
                    "error": "failed to read body"
                }));
            }
        };

        // Parse the JSON-RPC request envelope.
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

        // Extract the standard JSON-RPC 2.0 fields.
        //
        // - `id`: Request identifier.  Notifications have no id.  We pass
        //   it through so the response matches the request.
        // - `method`: The MCP method name (e.g., "initialize", "tools/call")
        // - `params`: Method-specific parameters (optional)
        let id = json.get("id").and_then(|v| v.as_u64());
        let method = json["method"].as_str().unwrap_or("");
        let params = json.get("params").cloned();

        tracing::debug!(method = method, id = ?id, "MCP server: handling request");

        let response = self.dispatch(id, method, params).await;
        json_response(StatusCode::OK, &response)
    }

    /// Dispatch a JSON-RPC method to the appropriate handler.
    ///
    /// ## Supported methods
    ///
    /// | Method                      | Handler              | Description |
    /// |-----------------------------|----------------------|-------------|
    /// | `initialize`                | `handle_initialize`  | MCP handshake — return server capabilities |
    /// | `notifications/initialized` | `handle_notification`| Post-handshake acknowledgment |
    /// | `tools/list`                | `handle_tools_list`  | Return workspace tool definitions |
    /// | `tools/call`                | `handle_tools_call`  | Execute a workspace tool |
    ///
    /// Unknown methods return JSON-RPC error code -32601 (Method not found).
    ///
    /// ## Why this set of methods?
    ///
    /// These are the minimum methods required by the MCP specification for
    /// a tool-providing server.  `initialize` + `notifications/initialized`
    /// perform the capability negotiation handshake.  `tools/list` lets the
    /// client discover available tools.  `tools/call` executes them.
    ///
    /// We don't implement `resources/*`, `prompts/*`, or `sampling/*`
    /// because we only expose tools, not other MCP primitives.
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

    /// Handle the `initialize` method — the first message in the MCP handshake.
    ///
    /// Returns the server's capabilities and identity.  We declare:
    /// - `protocolVersion`: "2024-11-05" (the MCP spec version we implement)
    /// - `capabilities.tools`: `{}` (we provide tools — the empty object
    ///   signals "yes, I have tools; call tools/list to discover them")
    /// - `serverInfo`: name + version for debugging
    ///
    /// The client (Claude Code) uses this to confirm protocol compatibility
    /// and to know which MCP features the server supports.
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

    /// Handle the `notifications/initialized` notification.
    ///
    /// This is the second message in the MCP handshake.  The client sends
    /// it after processing the `initialize` response to signal that it's
    /// ready to proceed.  Per the MCP spec, notifications don't require a
    /// response (no `id`), but Claude Code sends one anyway, so we return
    /// an empty result to keep things clean.
    fn handle_notification(&self, id: Option<u64>) -> JsonRpcResponse {
        JsonRpcResponse {
            id,
            result: Some(serde_json::json!({})),
            error: None,
        }
    }

    /// Handle `tools/list` — return definitions for all workspace tools.
    ///
    /// Converts each `Arc<dyn Tool>` into an `McpToolDef` containing:
    /// - `name`: The tool's unique identifier (e.g., "workspace_view")
    /// - `description`: Human-readable description (shown to the LLM)
    /// - `inputSchema`: JSON Schema for the tool's parameters
    ///
    /// Claude Code uses this response to register the tools as available
    /// capabilities.  The LLM then sees these tool definitions and can
    /// decide to call them via `tools/call`.
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

    /// Handle `tools/call` — execute a workspace tool and return its output.
    ///
    /// ## MCP tools/call request format
    ///
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 3,
    ///   "method": "tools/call",
    ///   "params": {
    ///     "name": "workspace_view",
    ///     "arguments": { "file": "SOUL.md" }
    ///   }
    /// }
    /// ```
    ///
    /// ## MCP tools/call response format
    ///
    /// ```json
    /// {
    ///   "id": 3,
    ///   "result": {
    ///     "content": [{ "type": "text", "text": "# Agent Soul\n..." }],
    ///     "isError": false
    ///   }
    /// }
    /// ```
    ///
    /// ## Processing steps
    ///
    /// 1. Validate `params` exists and contains a `name` field
    /// 2. Look up the tool by name in the HashMap
    /// 3. Extract `arguments` (default to `{}` if missing)
    /// 4. Build a `ToolContext` with the shared workspace reference
    /// 5. Call `tool.run(arguments, &ctx)` — the same method Dyson's
    ///    agent loop uses for any tool
    /// 6. Wrap the `ToolOutput` in MCP content blocks
    ///
    /// ## Error handling
    ///
    /// - Missing params → JSON-RPC error -32602 (Invalid params)
    /// - Missing tool name → JSON-RPC error -32602
    /// - Unknown tool → MCP tool error (isError: true in result body)
    /// - Tool execution failure → MCP tool error with error message
    ///
    /// Note the distinction: protocol validation errors use JSON-RPC error
    /// codes; tool-level failures use the MCP `isError` field in the
    /// result.  This matches the MCP spec's error model.
    ///
    /// ## Sandbox hook
    ///
    /// There is a placeholder for future sandbox enforcement between
    /// steps 4 and 5.  When `dangerous_no_sandbox` is false, a sandbox
    /// could inspect the tool name and arguments and deny execution.
    /// Today this is a no-op because workspace tools are in-memory.
    async fn handle_tools_call(
        &self,
        id: Option<u64>,
        params: Option<serde_json::Value>,
    ) -> JsonRpcResponse {
        // -- Validate the request --

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

        // -- Look up the tool --

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

        // -- Build the tool execution context --
        //
        // ToolContext is the same struct used everywhere in Dyson.  We
        // provide the workspace reference so workspace tools can access
        // the shared state.  The working_dir and env are defaults since
        // workspace tools don't use them (they're for BashTool).
        let ctx = ToolContext {
            working_dir: std::env::current_dir().unwrap_or_default(),
            env: HashMap::new(),
            cancellation: CancellationToken::new(),
            workspace: Some(Arc::clone(&self.workspace)),
        };

        // -- Sandbox hook (future) --
        //
        // When dangerous_no_sandbox is false, a sandbox implementation
        // could gate this call before execution.  For now workspace tools
        // are pure in-memory operations and don't need sandboxing.

        // -- Execute the tool and format the response --
        //
        // tool.run() returns Result<ToolOutput>.  We map both cases to
        // MCP response format: Ok → content + isError from ToolOutput,
        // Err → content with error message + isError: true.
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

/// Build an HTTP response with a JSON body.
///
/// Serializes `body` to JSON and returns an HTTP response with
/// `Content-Type: application/json` and the specified status code.
///
/// Used by all handlers to produce consistent response formatting.
/// The `unwrap()` on `Response::builder()` is safe because we're
/// constructing a response with valid, hardcoded headers.
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
//
// Strategy:
//   - Unit tests exercise the dispatch layer directly (no HTTP involved)
//     by calling `server.dispatch(id, method, params)`.  This tests the
//     JSON-RPC routing, parameter validation, tool execution, and error
//     handling without network overhead.
//   - Integration test (`server_binds_and_accepts`) starts the real HTTP
//     server and sends a request via reqwest, validating the full stack
//     from TCP accept to JSON response.
//
// MockWorkspace:
//   A minimal in-memory Workspace implementation with one file ("identity").
//   Just enough to verify that tools/call executes correctly and returns
//   workspace content.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal in-memory workspace for testing MCP server behavior.
    ///
    /// Pre-loaded with a single file `"identity"` containing `"I am a test agent"`.
    /// Implements the full `Workspace` trait with HashMap-backed storage.
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

    /// Helper to create a server with a MockWorkspace for testing.
    ///
    /// Uses `dangerous_no_sandbox: true` since tests don't need sandbox gating.
    fn make_server() -> Arc<McpHttpServer> {
        let ws: Arc<RwLock<Box<dyn Workspace>>> =
            Arc::new(RwLock::new(Box::new(MockWorkspace::new())));
        Arc::new(McpHttpServer::new(ws, true))
    }

    // -----------------------------------------------------------------------
    // MCP handshake tests
    // -----------------------------------------------------------------------

    /// Verify that `initialize` returns MCP protocol version, capabilities
    /// (declaring tool support), and server info.
    #[tokio::test]
    async fn initialize_returns_capabilities() {
        let server = make_server();
        let resp = server.dispatch(Some(1), "initialize", None).await;
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert!(result["capabilities"]["tools"].is_object());
    }

    // -----------------------------------------------------------------------
    // Tool discovery tests
    // -----------------------------------------------------------------------

    /// Verify that `tools/list` returns exactly the three workspace tools
    /// with their names matching the Tool trait implementations.
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

    // -----------------------------------------------------------------------
    // Tool execution tests
    // -----------------------------------------------------------------------

    /// Verify that `tools/call` with workspace_view actually reads from
    /// the workspace and returns the file content in MCP format.
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

    /// Verify that calling a nonexistent tool returns an MCP tool error
    /// (isError: true in the result), NOT a JSON-RPC error.  This is
    /// important — MCP treats unknown tools as application-level errors.
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
        assert!(resp.error.is_none()); // No JSON-RPC error
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true); // Tool-level error in result body
    }

    // -----------------------------------------------------------------------
    // Error handling tests
    // -----------------------------------------------------------------------

    /// Verify that an unrecognized method returns JSON-RPC error -32601.
    #[tokio::test]
    async fn unknown_method_returns_error() {
        let server = make_server();
        let resp = server.dispatch(Some(5), "bogus/method", None).await;
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    // -----------------------------------------------------------------------
    // Integration tests
    // -----------------------------------------------------------------------

    /// Full HTTP round-trip test: start the server, send a real HTTP
    /// request via reqwest, verify the JSON response.
    ///
    /// This tests the entire stack: TCP bind → accept → hyper HTTP/1.1
    /// → service_fn → handle_request → dispatch → serialize → respond.
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
