// ===========================================================================
// MCP transports — stdio and HTTP communication with MCP servers.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Provides two transport implementations for communicating with MCP
//   servers over JSON-RPC 2.0:
//
//   1. StdioTransport — spawn a local process, talk over stdin/stdout
//   2. HttpTransport  — POST JSON-RPC to an HTTP endpoint (streamable)
//
// The McpTransport trait:
//   Both transports implement `send_request()` and `send_notification()`.
//   The McpSkill doesn't know which transport it's using — it just calls
//   `transport.send_request("tools/call", params)`.
//
// Stdio transport:
//
//   Dyson (client)              MCP Server (child process)
//     │── spawn ──────────────> │
//     │── stdin: {"jsonrpc":"2.0","id":1,"method":"initialize",...}\n
//     │ <── stdout: {"jsonrpc":"2.0","id":1,"result":{...}}\n
//
//   Each message is a single JSON line.  A background task reads stdout
//   and dispatches responses to waiting callers by request ID.
//
// HTTP transport (Streamable HTTP MCP):
//
//   Dyson (client)              MCP Server (HTTP endpoint)
//     │── POST {"jsonrpc":"2.0","id":1,"method":"initialize",...}
//     │ <── 200 {"jsonrpc":"2.0","id":1,"result":{...}}
//
//   Each request is a separate HTTP POST.  The response body contains
//   the JSON-RPC response.  Headers (like API keys) are sent with every
//   request.  The server may return a Mcp-Session-Id header for session
//   tracking.
//
//   This is the "Streamable HTTP" transport from the MCP spec — simpler
//   than SSE, used by servers like Context7, Stripe, etc.
// ===========================================================================

use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};

use crate::error::{DysonError, Result};

use super::protocol::{JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};

use serde_json::Value;

// ---------------------------------------------------------------------------
// MCP payload caps
// ---------------------------------------------------------------------------

/// One mebibyte.
pub(crate) const MIB: usize = 1024 * 1024;

/// Outbound MCP request JSON cap.
pub(crate) const MAX_MCP_REQUEST_BYTES: usize = 16 * MIB;

/// Inbound MCP result cap, matched to Swarm's MCP runtime/proxy default.
pub(crate) const MAX_MCP_RESULT_BYTES: usize = 64 * MIB;

pub(crate) const MCP_RESULT_TOO_LARGE_MESSAGE: &str =
    "MCP result too large: MAX_MCP_RESULT_BYTES exceeded 64 MiB cap";

const MCP_REQUEST_TOO_LARGE_MESSAGE: &str =
    "MCP request too large: MAX_MCP_REQUEST_BYTES exceeded 16 MiB cap";

type PendingResponse = std::result::Result<JsonRpcResponse, String>;

fn enforce_mcp_request_cap(server: &str, json: &str) -> Result<()> {
    if json.len() > MAX_MCP_REQUEST_BYTES {
        return Err(DysonError::Mcp {
            server: server.to_string(),
            message: MCP_REQUEST_TOO_LARGE_MESSAGE.to_string(),
        });
    }
    Ok(())
}

fn mcp_result_too_large(server: &str) -> DysonError {
    DysonError::Mcp {
        server: server.to_string(),
        message: MCP_RESULT_TOO_LARGE_MESSAGE.to_string(),
    }
}

fn result_too_large_io_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        MCP_RESULT_TOO_LARGE_MESSAGE.to_string(),
    )
}

async fn read_line_capped<R>(reader: &mut R, max_bytes: usize) -> io::Result<Option<String>>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }

        if let Some(newline) = available.iter().position(|b| *b == b'\n') {
            if bytes.len().saturating_add(newline) > max_bytes {
                return Err(result_too_large_io_error());
            }
            bytes.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            if bytes.last().is_some_and(|b| *b == b'\r') {
                bytes.pop();
            }
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }

        if bytes.len().saturating_add(available.len()) > max_bytes {
            return Err(result_too_large_io_error());
        }
        let consumed = available.len();
        bytes.extend_from_slice(available);
        reader.consume(consumed);
    }
}

async fn read_response_text_capped(
    mut response: reqwest::Response,
    server: &str,
) -> Result<String> {
    if response
        .content_length()
        .is_some_and(|len| usize::try_from(len).map_or(true, |len| len > MAX_MCP_RESULT_BYTES))
    {
        return Err(mcp_result_too_large(server));
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| DysonError::Mcp {
        server: server.to_string(),
        message: format!("failed to read response: {e}"),
    })? {
        if body.len().saturating_add(chunk.len()) > MAX_MCP_RESULT_BYTES {
            return Err(mcp_result_too_large(server));
        }
        body.extend_from_slice(&chunk);
    }

    String::from_utf8(body).map_err(|e| DysonError::Mcp {
        server: server.to_string(),
        message: format!("failed to decode response as UTF-8: {e}"),
    })
}

// ---------------------------------------------------------------------------
// McpTransport trait
// ---------------------------------------------------------------------------

/// Abstract transport for MCP JSON-RPC communication.
///
/// Both stdio and HTTP implement this.  The McpSkill only sees this trait.
///
/// MCP is bidirectional: after `initialize`, the *server* may originate
/// its own requests (`sampling/createMessage`, `roots/list`,
/// `elicitation/create`) and notifications (`notifications/progress`,
/// `notifications/message`, `notifications/*/list_changed`).  The
/// transport's background reader routes that server-originated traffic to
/// an [`InboundHandler`] installed via [`McpTransport::set_inbound_handler`];
/// it still correlates responses to our own outbound requests by id.
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// Send a JSON-RPC request and wait for the response.
    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value>;

    /// Send a JSON-RPC notification (no response expected).
    async fn send_notification(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<()>;

    /// Install the handler for server-originated requests and
    /// notifications.  Until one is installed the transport answers
    /// inbound requests with `-32601 Method not found` and drops inbound
    /// notifications — the safe default for a peer that advertised no
    /// client capabilities.
    ///
    /// Default impl is a no-op so transports that cannot carry
    /// server-originated traffic (or don't yet) compile unchanged.
    fn set_inbound_handler(&self, _handler: Arc<dyn InboundHandler>) {}
}

/// Routes JSON-RPC messages that the MCP *server* originates (server →
/// client).  Implemented by the skill's notification/request router and
/// installed on the transport with [`McpTransport::set_inbound_handler`].
#[async_trait]
pub trait InboundHandler: Send + Sync {
    /// Handle a server-originated request.  The returned `Value` becomes
    /// the JSON-RPC `result`; an `Err` becomes the `error` object.
    async fn handle_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> std::result::Result<Value, JsonRpcError>;

    /// Handle a server-originated notification (no response).
    async fn handle_notification(&self, method: &str, params: Option<Value>);
}

/// Inbound handler installed by default — before a skill wires in its own
/// router.  Rejects server-originated requests and ignores notifications.
pub struct UnsupportedInboundHandler;

#[async_trait]
impl InboundHandler for UnsupportedInboundHandler {
    async fn handle_request(
        &self,
        method: &str,
        _params: Option<Value>,
    ) -> std::result::Result<Value, JsonRpcError> {
        Err(JsonRpcError {
            code: -32601,
            message: format!("Method not found: {method}"),
            data: None,
        })
    }

    async fn handle_notification(&self, _method: &str, _params: Option<Value>) {}
}

/// Classify an inbound JSON-RPC line as a response (to one of our
/// outbound requests), a server-originated request, or a server-originated
/// notification.  JSON-RPC distinguishes them structurally:
///   * `method` present + `id` present  → request
///   * `method` present + `id` absent    → notification
///   * `method` absent                   → response (matched by `id`)
///
/// Inbound request/notification ids are kept as raw `Value` because a
/// server may use string ids; we only need to echo the id back verbatim.
enum Inbound {
    Response,
    Request { id: Value, method: String, params: Option<Value> },
    Notification { method: String, params: Option<Value> },
}

fn classify_inbound(value: &Value) -> Inbound {
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return Inbound::Response;
    };
    let method = method.to_string();
    let params = value.get("params").cloned();
    match value.get("id") {
        Some(id) if !id.is_null() => Inbound::Request {
            id: id.clone(),
            method,
            params,
        },
        _ => Inbound::Notification { method, params },
    }
}

/// Serialize a JSON-RPC response to a server-originated request, echoing
/// its id verbatim (string or number).
fn build_response_line(id: &Value, result: std::result::Result<Value, JsonRpcError>) -> String {
    let body = match result {
        Ok(result) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(err) => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": err.code, "message": err.message, "data": err.data },
        }),
    };
    body.to_string()
}

// ---------------------------------------------------------------------------
// StdioTransport
// ---------------------------------------------------------------------------

/// Manages a stdio connection to an MCP server process.
///
/// Spawns the process, sends requests via stdin, reads responses from
/// stdout.  A background task matches responses to waiting callers by ID.
pub struct StdioTransport {
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<PendingResponse>>>>,
    /// Handler for server-originated requests/notifications.  Shared with
    /// the reader task; swapped in by `set_inbound_handler`.  A `std`
    /// mutex is fine: it is only ever held to clone the `Arc` out, never
    /// across an `.await`.
    inbound: Arc<std::sync::Mutex<Arc<dyn InboundHandler>>>,
    _child: Arc<Mutex<Child>>,
    /// Background task handles — aborted on drop to prevent orphaned tasks.
    _reader_handle: tokio::task::JoinHandle<()>,
}

impl StdioTransport {
    /// Spawn an MCP server process and establish communication.
    ///
    /// When `sandbox` is true on Linux and `bwrap` is on PATH, the
    /// subprocess is wrapped with `bwrap` using a read-only root,
    /// tmpfs `/tmp`, PID-namespace isolation, and `--die-with-parent`.
    /// If `deny_network` is also true, the network namespace is
    /// unshared so the server cannot reach the network.
    ///
    /// On non-Linux or when bwrap is missing, `sandbox = true` falls
    /// back to the unsandboxed path with a warning.
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        sandbox: bool,
        deny_network: bool,
    ) -> Result<Self> {
        let (exec, exec_args) = Self::resolve_exec(command, args, sandbox, deny_network)?;
        let mut cmd = Command::new(&exec);
        cmd.args(&exec_args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().map_err(|e| DysonError::Mcp {
            server: command.to_string(),
            message: format!("failed to spawn: {e}"),
        })?;

        let stdin = child.stdin.take().ok_or_else(|| DysonError::Mcp {
            server: command.to_string(),
            message: "failed to open stdin".into(),
        })?;

        let stdout = child.stdout.take().ok_or_else(|| DysonError::Mcp {
            server: command.to_string(),
            message: "failed to open stdout".into(),
        })?;

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<PendingResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let inbound: Arc<std::sync::Mutex<Arc<dyn InboundHandler>>> =
            Arc::new(std::sync::Mutex::new(Arc::new(UnsupportedInboundHandler)));

        // Background reader task.
        let pending_clone = Arc::clone(&pending);
        let inbound_clone = Arc::clone(&inbound);
        let stdin_clone = Arc::clone(&stdin);
        let command_name = command.to_string();
        let reader_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);

            loop {
                let line = match read_line_capped(&mut reader, MAX_MCP_RESULT_BYTES).await {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(e) => {
                        let message = e.to_string();
                        tracing::warn!(
                            server = command_name,
                            error = %message,
                            "failed to read MCP server output"
                        );
                        let mut pending = pending_clone.lock().await;
                        for (_, tx) in pending.drain() {
                            let _ = tx.send(Err(message.clone()));
                        }
                        break;
                    }
                };
                if line.is_empty() {
                    continue;
                }

                let value: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!(
                            server = command_name,
                            error = %e,
                            "failed to parse MCP server output"
                        );
                        continue;
                    }
                };

                match classify_inbound(&value) {
                    Inbound::Response => {
                        let response: JsonRpcResponse = match serde_json::from_value(value) {
                            Ok(r) => r,
                            Err(e) => {
                                tracing::debug!(
                                    server = command_name,
                                    error = %e,
                                    "failed to parse MCP response frame"
                                );
                                continue;
                            }
                        };
                        if let Some(id) = response.id {
                            let mut pending = pending_clone.lock().await;
                            if let Some(tx) = pending.remove(&id) {
                                let _ = tx.send(Ok(response));
                            }
                        }
                    }
                    // Server-originated request: dispatch to the handler on
                    // its own task (the handler may block — e.g. an
                    // elicitation awaiting user input — and must not stall
                    // the reader) and write the response back over stdin.
                    Inbound::Request { id, method, params } => {
                        let handler = Arc::clone(&inbound_clone.lock().unwrap());
                        let stdin = Arc::clone(&stdin_clone);
                        let server = command_name.clone();
                        tokio::spawn(async move {
                            let result = handler.handle_request(&method, params).await;
                            let line = build_response_line(&id, result);
                            if let Err(e) = write_line(&stdin, &server, &line).await {
                                tracing::warn!(
                                    server = server,
                                    error = %e,
                                    "failed to write inbound MCP response"
                                );
                            }
                        });
                    }
                    Inbound::Notification { method, params } => {
                        let handler = Arc::clone(&inbound_clone.lock().unwrap());
                        tokio::spawn(async move {
                            handler.handle_notification(&method, params).await;
                        });
                    }
                }
            }

            tracing::debug!(server = command_name, "MCP server stdout closed");
        });

        Ok(Self {
            stdin,
            next_id: AtomicU64::new(1),
            pending,
            inbound,
            _child: Arc::new(Mutex::new(child)),
            _reader_handle: reader_handle,
        })
    }
}

/// Write a single newline-terminated JSON line to an MCP server's stdin.
/// Shared by outbound requests/notifications and inbound responses.
async fn write_line(
    stdin: &Arc<Mutex<tokio::process::ChildStdin>>,
    server: &str,
    json: &str,
) -> Result<()> {
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(json.as_bytes())
        .await
        .map_err(|e| DysonError::Mcp {
            server: server.to_string(),
            message: format!("stdin write failed: {e}"),
        })?;
    stdin.write_all(b"\n").await.map_err(|e| DysonError::Mcp {
        server: server.to_string(),
        message: format!("stdin write failed: {e}"),
    })?;
    stdin.flush().await.map_err(|e| DysonError::Mcp {
        server: server.to_string(),
        message: format!("stdin flush failed: {e}"),
    })?;
    Ok(())
}

impl StdioTransport {
    /// Decide whether to invoke the MCP command directly or wrap it in
    /// `bwrap`.  Returns `(exec, argv)` suitable for `Command::new` +
    /// `.args()`.
    ///
    /// When `sandbox == true` and no wrapper is available (bwrap
    /// missing on Linux, no wrapper on non-Linux), returns an error
    /// rather than falling back to a direct unsandboxed spawn — that
    /// fallback was a silent sandbox bypass.
    fn resolve_exec(
        command: &str,
        args: &[String],
        sandbox: bool,
        deny_network: bool,
    ) -> Result<(String, Vec<String>)> {
        #[cfg(target_os = "linux")]
        let has_bwrap = std::process::Command::new("sh")
            .arg("-c")
            .arg("command -v bwrap")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        #[cfg(not(target_os = "linux"))]
        let has_bwrap = false;

        Self::resolve_exec_with(command, args, sandbox, has_bwrap, deny_network)
    }

    /// Pure decision function — extracted so the bwrap-missing path
    /// is testable without depending on the host's PATH.
    fn resolve_exec_with(
        command: &str,
        args: &[String],
        sandbox: bool,
        has_bwrap: bool,
        deny_network: bool,
    ) -> Result<(String, Vec<String>)> {
        if !sandbox {
            return Ok((command.to_string(), args.to_vec()));
        }
        if has_bwrap {
            let argv =
                crate::sandbox::os::build_bwrap_argv_for_mcp_stdio(command, args, deny_network);
            return Ok(("bwrap".to_string(), argv));
        }
        Err(DysonError::Mcp {
            server: command.to_string(),
            message: "sandbox requested but bwrap is unavailable on this host; \
                      install bubblewrap or run with --dangerous-no-sandbox to \
                      accept an unsandboxed MCP stdio process"
                .into(),
        })
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self._reader_handle.abort();
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest::new(id, method, params);

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, tx);
        }

        let json = serde_json::to_string(&request)?;
        enforce_mcp_request_cap(method, &json)?;
        write_line(&self.stdin, method, &json).await?;

        /// Maximum time to wait for an MCP server to respond to a request.
        const MCP_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

        let response = match tokio::time::timeout(MCP_RESPONSE_TIMEOUT, rx).await {
            Ok(Ok(Ok(resp))) => resp,
            Ok(Ok(Err(message))) => {
                self.pending.lock().await.remove(&id);
                return Err(DysonError::Mcp {
                    server: method.to_string(),
                    message,
                });
            }
            Ok(Err(_)) => {
                // Channel closed — server died. Clean up the pending entry.
                self.pending.lock().await.remove(&id);
                return Err(DysonError::Mcp {
                    server: method.to_string(),
                    message: "response channel closed (server died?)".into(),
                });
            }
            Err(_) => {
                // Timeout — clean up the orphaned pending entry.
                self.pending.lock().await.remove(&id);
                return Err(DysonError::Mcp {
                    server: method.to_string(),
                    message: format!(
                        "response timed out after {}s",
                        MCP_RESPONSE_TIMEOUT.as_secs()
                    ),
                });
            }
        };

        if let Some(err) = response.error {
            return Err(DysonError::Mcp {
                server: method.to_string(),
                message: format!("RPC error {}: {}", err.code, err.message),
            });
        }

        response.result.ok_or_else(|| DysonError::Mcp {
            server: method.to_string(),
            message: "response has neither result nor error".into(),
        })
    }

    async fn send_notification(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<()> {
        let notification = JsonRpcNotification::new(method, params);
        let json = serde_json::to_string(&notification)?;
        enforce_mcp_request_cap(method, &json)?;
        write_line(&self.stdin, method, &json).await
    }

    fn set_inbound_handler(&self, handler: Arc<dyn InboundHandler>) {
        *self.inbound.lock().unwrap() = handler;
    }
}

// ---------------------------------------------------------------------------
// HttpTransport — Streamable HTTP MCP
// ---------------------------------------------------------------------------

/// Communicates with an MCP server over HTTP POST requests.
///
/// Each JSON-RPC message is sent as the body of a POST request.  The
/// response body contains the JSON-RPC response.  Custom headers (like
/// API keys) are sent with every request.
///
/// Used by MCP servers like Context7 that expose an HTTP endpoint
/// instead of running as a local process.
///
/// ```json
/// // In dyson.json:
/// "mcp_servers": {
///   "context7": {
///     "url": "https://mcp.context7.com/mcp",
///     "headers": { "CONTEXT7_API_KEY": "your-key" }
///   }
/// }
/// ```
pub struct HttpTransport {
    /// HTTP client (connection pooling, TLS).
    client: reqwest::Client,

    /// The MCP server URL to POST to.
    url: String,

    /// Authentication handler for outgoing requests.
    /// Applies headers (API keys, auth tokens, etc.) via the Auth trait.
    auth: Box<dyn crate::auth::Auth>,

    /// Request ID counter.
    next_id: AtomicU64,

    /// Session ID returned by the server (if any).
    /// Some MCP HTTP servers return a session ID in the response headers
    /// that must be sent back in subsequent requests.
    session_id: Mutex<Option<String>>,

    /// Handler for server-originated requests/notifications that arrive
    /// interleaved in an SSE response body.  Defaults to
    /// [`UnsupportedInboundHandler`] until a skill installs its router.
    inbound: std::sync::Mutex<Arc<dyn InboundHandler>>,
}

impl HttpTransport {
    /// Create a new HTTP transport.
    ///
    /// `url` is the MCP server endpoint.
    /// `auth` applies authentication headers to every request.
    pub fn new(url: &str, auth: Box<dyn crate::auth::Auth>) -> Self {
        Self {
            client: crate::http::client().clone(),
            url: url.to_string(),
            auth,
            next_id: AtomicU64::new(1),
            session_id: Mutex::new(None),
            inbound: std::sync::Mutex::new(Arc::new(UnsupportedInboundHandler)),
        }
    }

    /// POST a fire-and-forget JSON-RPC message (a notification, or our
    /// response to a server-originated request) to the MCP endpoint.
    /// Streamable HTTP servers accept these as ordinary POSTs and reply
    /// 202 Accepted with no body, so we don't read the response.
    async fn post_message(&self, json: &str) {
        let Ok(mut req) = self.build_request(json).await else {
            return;
        };
        {
            let session = self.session_id.lock().await;
            if let Some(ref sid) = *session {
                req = req.header("Mcp-Session-Id", sid.as_str());
            }
        }
        let _ = req.send().await;
    }

    /// Dispatch one server-originated frame seen in an SSE response body:
    /// a request gets routed to the inbound handler and its response
    /// POSTed back; a notification is routed and dropped.  Returns the
    /// value verbatim for the caller to treat as the response to our
    /// outbound request when it is neither (i.e. a `Response` frame).
    async fn dispatch_inbound_frame(&self, value: Value) -> Option<Value> {
        match classify_inbound(&value) {
            Inbound::Response => Some(value),
            Inbound::Request { id, method, params } => {
                let handler = Arc::clone(&self.inbound.lock().unwrap());
                let result = handler.handle_request(&method, params).await;
                self.post_message(&build_response_line(&id, result)).await;
                None
            }
            Inbound::Notification { method, params } => {
                let handler = Arc::clone(&self.inbound.lock().unwrap());
                handler.handle_notification(&method, params).await;
                None
            }
        }
    }

    /// Build a request with common headers.
    async fn build_request(&self, body: &str) -> crate::error::Result<reqwest::RequestBuilder> {
        let req = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(body.to_string());

        // Apply auth headers (API keys, bearer tokens, etc.).
        self.auth.apply_to_request(req).await
    }

    /// Send an HTTP request and capture the session ID from the response.
    ///
    /// Shared by the normal path and the 401 retry path to avoid duplication.
    async fn send_http(&self, json: &str) -> crate::error::Result<reqwest::Response> {
        let mut req = self.build_request(json).await?;

        {
            let session = self.session_id.lock().await;
            if let Some(ref sid) = *session {
                req = req.header("Mcp-Session-Id", sid.as_str());
            }
        }

        let response = req.send().await.map_err(|e| DysonError::Mcp {
            server: self.url.clone(),
            message: format!("HTTP request failed: {e}"),
        })?;

        if let Some(sid) = response.headers().get("mcp-session-id")
            && let Ok(sid_str) = sid.to_str()
        {
            let mut session = self.session_id.lock().await;
            *session = Some(sid_str.to_string());
        }

        Ok(response)
    }

    /// Parse an HTTP response into a JSON-RPC result.
    ///
    /// Streamable HTTP MCP servers may return either `application/json`
    /// (single response in the body) or `text/event-stream` (one or
    /// more `event: message\ndata: <json>\n\n` frames).  Context7,
    /// GitHub MCP, Linear, and most reference servers prefer SSE — the
    /// initial-only-JSON path was the previous behavior and silently
    /// dropped any SSE-shaped response with "expected value at line 1".
    ///
    /// For SSE bodies the parse is *incremental*: each complete `data:`
    /// frame is dispatched as soon as its terminating blank line
    /// arrives, so a server-originated `elicitation/create` reaches
    /// the inbound handler — and its answer goes out on a separate
    /// POST — before the upstream tools/call finishes.  That's the
    /// difference between "elicitation works" and "elicitation
    /// deadlocks waiting on the body to close."
    ///
    /// Shared by the normal path and the 401 retry path.
    async fn parse_rpc_response(
        &self,
        response: reqwest::Response,
    ) -> crate::error::Result<serde_json::Value> {
        if !response.status().is_success() {
            let status = response.status();
            let body = match read_response_text_capped(response, &self.url).await {
                Ok(body) => body,
                Err(e) if e.to_string().contains(MCP_RESULT_TOO_LARGE_MESSAGE) => return Err(e),
                Err(_) => "failed to read body".into(),
            };
            return Err(DysonError::Mcp {
                server: self.url.clone(),
                message: format!("HTTP {status}: {body}"),
            });
        }

        let is_sse = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| {
                ct.split(';')
                    .next()
                    .is_some_and(|m| m.trim().eq_ignore_ascii_case("text/event-stream"))
            });

        if !is_sse {
            // Non-SSE: the whole body is the single JSON-RPC response,
            // safe to buffer.
            let body = read_response_text_capped(response, &self.url).await?;
            return self.finish_rpc_value(parse_rpc_value(&body, &self.url)?);
        }

        // SSE: stream the body, dispatching each complete frame as it
        // arrives so server-originated requests can be answered while
        // the body is still open.
        self.stream_sse_response(response).await
    }

    /// Incrementally parse an SSE response, dispatching each frame
    /// (response, server-originated request, notification) the moment
    /// its terminating blank line is observed.
    async fn stream_sse_response(
        &self,
        mut response: reqwest::Response,
    ) -> crate::error::Result<serde_json::Value> {
        let mut parser = SseStreamParser::new();
        let mut total_bytes = 0usize;
        let mut answer: Option<Value> = None;
        while let Some(chunk) = response.chunk().await.map_err(|e| DysonError::Mcp {
            server: self.url.clone(),
            message: format!("failed to read SSE chunk: {e}"),
        })? {
            total_bytes = total_bytes.saturating_add(chunk.len());
            if total_bytes > MAX_MCP_RESULT_BYTES {
                return Err(mcp_result_too_large(&self.url));
            }
            let new_frames = parser.feed(&chunk);
            for frame in new_frames {
                let value = parse_rpc_value(&frame, &self.url)?;
                if let Some(resp) = self.dispatch_inbound_frame(value).await {
                    answer = Some(resp);
                }
            }
        }
        // Flush any trailing frame the server forgot to terminate with
        // a blank line.
        if let Some(frame) = parser.finish() {
            let value = parse_rpc_value(&frame, &self.url)?;
            if let Some(resp) = self.dispatch_inbound_frame(value).await {
                answer = Some(resp);
            }
        }

        match answer {
            Some(v) => self.finish_rpc_value(v),
            None => Err(DysonError::Mcp {
                server: self.url.clone(),
                message: "SSE response carried no response to our request".into(),
            }),
        }
    }

    /// Turn a parsed JSON-RPC response value into the `result`, mapping a
    /// JSON-RPC `error` object onto a `DysonError`.
    fn finish_rpc_value(&self, value: Value) -> crate::error::Result<serde_json::Value> {
        let rpc_response: JsonRpcResponse =
            serde_json::from_value(value).map_err(|e| DysonError::Mcp {
                server: self.url.clone(),
                message: format!("failed to parse response: {e}"),
            })?;

        if let Some(err) = rpc_response.error {
            return Err(DysonError::Mcp {
                server: self.url.clone(),
                message: format!("RPC error {}: {}", err.code, err.message),
            });
        }

        rpc_response.result.ok_or_else(|| DysonError::Mcp {
            server: self.url.clone(),
            message: "response has neither result nor error".into(),
        })
    }
}

/// Parse one JSON-RPC frame (a single `data:` payload or a non-SSE body)
/// into a `Value`, attributing parse failures to `server`.
fn parse_rpc_value(raw: &str, server: &str) -> crate::error::Result<Value> {
    serde_json::from_str(raw).map_err(|e| DysonError::Mcp {
        server: server.to_string(),
        message: format!("failed to parse response: {e}"),
    })
}

/// Extract the last `data:` payload from a Server-Sent Events body.
///
/// Streamable HTTP MCP servers wrap the JSON-RPC response in an
/// `event: message\ndata: {...}\n\n` envelope (RFC-style SSE).  For
/// the request/response shape MCP uses for initialize / tools/list /
/// tools/call, exactly one frame is emitted — but defensively we
/// take the LAST `data:` line so future server behavior (a status
/// frame followed by the response, etc.) doesn't trip the parser.
#[cfg(test)]
fn extract_last_sse_data(body: &str) -> Option<String> {
    extract_all_sse_frames(body).pop()
}

/// Extract every `data:` payload from a Server-Sent Events body, in
/// order.  Retained as a test helper for fixture-based tests — the
/// production path uses [`SseStreamParser`] so dispatch happens as
/// each frame arrives, not after the body terminates.
#[cfg(test)]
fn extract_all_sse_frames(body: &str) -> Vec<String> {
    let mut frames: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            let chunk = rest.strip_prefix(' ').unwrap_or(rest);
            current = Some(match current.take() {
                Some(prev) => format!("{prev}\n{chunk}"),
                None => chunk.to_string(),
            });
        } else if line.is_empty()
            && let Some(c) = current.take()
        {
            frames.push(c);
        }
    }
    if let Some(c) = current {
        frames.push(c);
    }
    frames
}

/// Incremental SSE parser.  Accumulates bytes across reqwest chunks and
/// emits each complete `data:` event as soon as the terminating blank
/// line arrives, so the caller can dispatch frames before the rest of
/// the body has been read.
///
/// Matches [`extract_all_sse_frames`]'s semantics for buffered bodies:
/// every `data:` payload becomes one frame; multiple `data:` lines in
/// one event are joined with `\n`; the trailing blank line is optional
/// (we surface the final frame via [`SseStreamParser::finish`]).
struct SseStreamParser {
    buf: Vec<u8>,
    current: Option<String>,
}

impl SseStreamParser {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            current: None,
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(newline) = self.buf.iter().position(|b| *b == b'\n') {
            let raw_line: Vec<u8> = self.buf.drain(..=newline).collect();
            // Drop the trailing `\n` and any preceding `\r`.
            let mut len = raw_line.len() - 1;
            if len > 0 && raw_line[len - 1] == b'\r' {
                len -= 1;
            }
            let line = match std::str::from_utf8(&raw_line[..len]) {
                Ok(s) => s.to_string(),
                Err(_) => {
                    // Non-UTF8 line: skip it.  This matches the
                    // buffered parser's behaviour of dropping garbage
                    // lines silently.
                    continue;
                }
            };
            if let Some(rest) = line.strip_prefix("data:") {
                let chunk = rest.strip_prefix(' ').unwrap_or(rest);
                self.current = Some(match self.current.take() {
                    Some(prev) => format!("{prev}\n{chunk}"),
                    None => chunk.to_string(),
                });
            } else if line.is_empty()
                && let Some(frame) = self.current.take()
            {
                frames.push(frame);
            }
            // Other SSE lines (event:, id:, retry:, comments starting
            // with `:`) are intentionally ignored — the wire format
            // we accept is exactly the one Streamable HTTP MCP emits:
            // `event: message\ndata: <json>\n\n`.
        }
        frames
    }

    /// Flush a trailing frame the server omitted to terminate with a
    /// blank line.  Mirrors the lenient behaviour of
    /// [`extract_all_sse_frames`] / [`extract_last_sse_data`].
    fn finish(mut self) -> Option<String> {
        // Drain any unterminated line in `buf` as a tail event.
        if !self.buf.is_empty() {
            let raw = std::mem::take(&mut self.buf);
            if let Ok(line) = std::str::from_utf8(&raw) {
                let trimmed = line.strip_suffix('\r').unwrap_or(line);
                if let Some(rest) = trimmed.strip_prefix("data:") {
                    let chunk = rest.strip_prefix(' ').unwrap_or(rest);
                    self.current = Some(match self.current.take() {
                        Some(prev) => format!("{prev}\n{chunk}"),
                        None => chunk.to_string(),
                    });
                }
            }
        }
        self.current.take()
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest::new(id, method, params);
        let json = serde_json::to_string(&request)?;
        enforce_mcp_request_cap(&self.url, &json)?;

        let response = self.send_http(&json).await?;

        // 401 Unauthorized — refresh credentials and retry once.
        //
        // Handles OAuth token rejection (clock skew, server-side revocation)
        // when the token hasn't expired locally.  on_unauthorized() gives
        // OAuth a chance to force-refresh before we retry.
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            tracing::debug!(server = %self.url, "received 401 — attempting credential refresh");

            if self.auth.on_unauthorized().await.is_ok() {
                let retry_response = self.send_http(&json).await?;
                return self.parse_rpc_response(retry_response).await;
            }

            let body = match read_response_text_capped(response, &self.url).await {
                Ok(body) => body,
                Err(e) if e.to_string().contains(MCP_RESULT_TOO_LARGE_MESSAGE) => return Err(e),
                Err(_) => "failed to read body".into(),
            };
            return Err(DysonError::Mcp {
                server: self.url.clone(),
                message: format!("HTTP 401 Unauthorized: {body}"),
            });
        }

        self.parse_rpc_response(response).await
    }

    async fn send_notification(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<()> {
        let notification = JsonRpcNotification::new(method, params);
        let json = serde_json::to_string(&notification)?;
        enforce_mcp_request_cap(&self.url, &json)?;

        let mut req = self.build_request(&json).await?;

        {
            let session = self.session_id.lock().await;
            if let Some(ref sid) = *session {
                req = req.header("Mcp-Session-Id", sid.as_str());
            }
        }

        // Fire and forget — we don't check the response for notifications.
        let _ = req.send().await;

        Ok(())
    }

    fn set_inbound_handler(&self, handler: Arc<dyn InboundHandler>) {
        *self.inbound.lock().unwrap() = handler;
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawn a mock MCP server that echoes JSON-RPC responses.
    ///
    /// The script reads a JSON line from stdin, extracts the id, and
    /// writes back a JSON-RPC response with `{"ok": true}` as the result.
    async fn spawn_echo_server() -> StdioTransport {
        StdioTransport::spawn(
            "sh",
            &[
                "-c".to_string(),
                // Read lines, parse id with sed, echo a response.
                r#"while IFS= read -r line; do
                    id=$(echo "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                    echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"ok\":true}}"
                done"#
                    .to_string(),
            ],
            &HashMap::new(),
            false,
            false,
        )
        .await
        .expect("failed to spawn echo server")
    }

    #[test]
    fn classify_inbound_distinguishes_message_shapes() {
        // Response: no method, id present.
        let resp = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}});
        assert!(matches!(classify_inbound(&resp), Inbound::Response));

        // Request: method + non-null id (string id is legal JSON-RPC).
        let req = serde_json::json!({"jsonrpc":"2.0","id":"abc","method":"roots/list"});
        match classify_inbound(&req) {
            Inbound::Request { id, method, .. } => {
                assert_eq!(id, serde_json::json!("abc"));
                assert_eq!(method, "roots/list");
            }
            _ => panic!("expected request"),
        }

        // Notification: method, no id.
        let note = serde_json::json!({"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info"}});
        match classify_inbound(&note) {
            Inbound::Notification { method, params } => {
                assert_eq!(method, "notifications/message");
                assert_eq!(params.unwrap()["level"], "info");
            }
            _ => panic!("expected notification"),
        }

        // Null id + method is a notification, not a request (JSON-RPC
        // forbids null request ids).
        let null_id = serde_json::json!({"jsonrpc":"2.0","id":null,"method":"x"});
        assert!(matches!(classify_inbound(&null_id), Inbound::Notification { .. }));
    }

    #[test]
    fn build_response_line_echoes_id_and_shapes_result_or_error() {
        let ok = build_response_line(
            &serde_json::json!("r1"),
            Ok(serde_json::json!({"roots": []})),
        );
        let v: serde_json::Value = serde_json::from_str(&ok).unwrap();
        assert_eq!(v["id"], "r1");
        assert_eq!(v["result"]["roots"], serde_json::json!([]));
        assert!(v.get("error").is_none());

        let err = build_response_line(
            &serde_json::json!(7),
            Err(JsonRpcError { code: -32601, message: "nope".into(), data: None }),
        );
        let v: serde_json::Value = serde_json::from_str(&err).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["error"]["code"], -32601);
        assert_eq!(v["error"]["message"], "nope");
    }

    /// Records inbound dispatch for assertions and answers `roots/list`
    /// with a sentinel so the full request round-trip is observable.
    struct RecordingHandler {
        notes: tokio::sync::mpsc::UnboundedSender<(String, Option<Value>)>,
    }

    #[async_trait]
    impl InboundHandler for RecordingHandler {
        async fn handle_request(
            &self,
            method: &str,
            _params: Option<Value>,
        ) -> std::result::Result<Value, JsonRpcError> {
            assert_eq!(method, "roots/list");
            Ok(serde_json::json!({ "roots": ["sentinel"] }))
        }

        async fn handle_notification(&self, method: &str, params: Option<Value>) {
            let _ = self.notes.send((method.to_string(), params));
        }
    }

    #[tokio::test]
    async fn inbound_request_is_dispatched_and_response_written_back() {
        // Server waits for a kick, emits a server-originated `roots/list`
        // request, reads our response, then echoes it back wrapped in a
        // notification so the test can confirm the full round-trip:
        // dispatch -> handler -> response written to stdin -> server saw it.
        let transport = StdioTransport::spawn(
            "sh",
            &[
                "-c".to_string(),
                r#"IFS= read -r _go
                   echo '{"jsonrpc":"2.0","id":"r1","method":"roots/list"}'
                   IFS= read -r resp
                   printf '{"jsonrpc":"2.0","method":"notifications/echo","params":%s}\n' "$resp"
                   cat >/dev/null"#
                    .to_string(),
            ],
            &HashMap::new(),
            false,
            false,
        )
        .await
        .expect("spawn inbound test server");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        transport.set_inbound_handler(Arc::new(RecordingHandler { notes: tx }));

        // Kick the server (installs the handler first to avoid the race
        // where the default handler answers before ours is in place).
        transport.send_notification("go", None).await.unwrap();

        let (method, params) = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("echo notification within timeout")
            .expect("channel open");

        assert_eq!(method, "notifications/echo");
        // params is the response object the server received from us.
        let echoed = params.expect("echo params");
        assert_eq!(echoed["id"], "r1");
        assert_eq!(echoed["result"]["roots"][0], "sentinel");
    }

    #[tokio::test]
    async fn send_request_receives_matching_response() {
        let transport = spawn_echo_server().await;
        let result = transport.send_request("test/method", None).await.unwrap();
        assert_eq!(result, serde_json::json!({"ok": true}));
    }

    #[tokio::test]
    async fn stdio_stdout_frame_above_result_cap_fails_explicitly() {
        let mut reader = BufReader::new(tokio::io::repeat(b'a'));
        let err = read_line_capped(&mut reader, MAX_MCP_RESULT_BYTES)
            .await
            .expect_err("line above cap should fail");

        assert_eq!(err.to_string(), MCP_RESULT_TOO_LARGE_MESSAGE);
    }

    #[tokio::test]
    async fn http_response_above_result_cap_fails_explicitly() {
        use std::convert::Infallible;

        use http_body_util::StreamBody;
        use hyper::body::{Bytes, Frame, Incoming};
        use hyper::service::service_fn;
        use hyper::{Request, Response};
        use hyper_util::rt::TokioIo;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept connection");
            let io = TokioIo::new(stream);
            let service = service_fn(|_req: Request<Incoming>| async move {
                let body_stream = async_stream::stream! {
                    for _ in 0..=MAX_MCP_RESULT_BYTES / MIB {
                        yield Ok::<_, Infallible>(Frame::data(Bytes::from(vec![b'a'; MIB])));
                    }
                };
                Ok::<_, Infallible>(
                    Response::builder()
                        .header("content-type", "application/json")
                        .body(StreamBody::new(body_stream))
                        .expect("response"),
                )
            });

            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await;
        });

        let transport = HttpTransport::new(
            &format!("http://{addr}/mcp"),
            Box::new(crate::auth::DangerousNoAuth),
        );
        let err = transport
            .send_request("tools/call", Some(serde_json::json!({})))
            .await
            .expect_err("oversized HTTP MCP response should fail");
        server.abort();

        assert!(
            err.to_string().contains(MCP_RESULT_TOO_LARGE_MESSAGE),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extract_all_sse_frames_returns_every_frame_in_order() {
        let body = "event: message\ndata: {\"a\":1}\n\nevent: message\ndata: {\"b\":2}\n\n";
        let frames = extract_all_sse_frames(body);
        assert_eq!(frames, vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()]);
    }

    #[tokio::test]
    async fn http_sse_streams_inbound_then_unblocks_on_answer_post() {
        // End-to-end streaming regression: the server emits an elicit
        // frame, then *waits* for the dyson client to POST the answer
        // on a separate request, then emits the tools/call response
        // frame.  If `parse_rpc_response` ever buffers the SSE body
        // before dispatching, this test deadlocks — the server is
        // waiting on an answer that can't arrive until dispatch fires.
        //
        // Captures the wire-shape contract we depend on for the
        // swarm-proxied stdio elicit flow.
        use std::convert::Infallible;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        use http_body_util::StreamBody;
        use http_body_util::combinators::BoxBody;
        use hyper::body::{Bytes, Frame, Incoming};
        use hyper::service::service_fn;
        use hyper::{Request, Response};
        use hyper_util::rt::TokioIo;

        static ANSWER_SEEN: AtomicBool = AtomicBool::new(false);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            // Two connections expected: the streaming tools/call POST,
            // and the inbound answer POST.  Both are loopback so they
            // complete fast; the order is determined by the client's
            // dispatch behaviour, which is what we're asserting.
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.expect("accept");
                let io = TokioIo::new(stream);
                tokio::spawn(async move {
                    let svc = service_fn(|req: Request<Incoming>| async move {
                        // POST body tells us which call this is.
                        let body_bytes = http_body_util::BodyExt::collect(req.into_body())
                            .await
                            .unwrap()
                            .to_bytes();
                        let req_value: serde_json::Value =
                            serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);
                        // The inbound answer is a JSON-RPC response —
                        // id present, no method.
                        let is_inbound_answer = req_value.get("method").is_none()
                            && req_value.get("id").is_some();
                        // Single stream type for both branches so
                        // their `Response<...>` types unify.
                        let body_stream = async_stream::stream! {
                            if is_inbound_answer {
                                ANSWER_SEEN.store(true, Ordering::SeqCst);
                            } else {
                                yield Ok::<_, Infallible>(Frame::data(Bytes::from(
                                    "event: message\n\
                                     data: {\"jsonrpc\":\"2.0\",\"id\":\"e1\",\"method\":\"elicitation/create\",\"params\":{\"message\":\"go?\",\"requestedSchema\":{}}}\n\n",
                                )));
                                // Wait for the answer.  Polling is fine
                                // here — the dispatch is on a different
                                // connection so we won't deadlock.
                                for _ in 0..200 {
                                    if ANSWER_SEEN.load(Ordering::SeqCst) {
                                        break;
                                    }
                                    tokio::time::sleep(Duration::from_millis(25)).await;
                                }
                                yield Ok(Frame::data(Bytes::from(
                                    "event: message\n\
                                     data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n",
                                )));
                            }
                        };
                        let body = BoxBody::new(StreamBody::new(body_stream));
                        let status = if is_inbound_answer { 202 } else { 200 };
                        let mut builder = Response::builder().status(status);
                        if !is_inbound_answer {
                            builder = builder.header("content-type", "text/event-stream");
                        }
                        Ok::<_, Infallible>(builder.body(body).expect("response"))
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, svc)
                        .await;
                });
            }
        });

        struct ElicitHandler;
        #[async_trait]
        impl InboundHandler for ElicitHandler {
            async fn handle_request(
                &self,
                method: &str,
                _params: Option<Value>,
            ) -> std::result::Result<Value, JsonRpcError> {
                assert_eq!(method, "elicitation/create");
                Ok(serde_json::json!({ "action": "accept", "content": {} }))
            }
            async fn handle_notification(&self, _method: &str, _params: Option<Value>) {}
        }

        let transport = HttpTransport::new(
            &format!("http://{addr}/mcp"),
            Box::new(crate::auth::DangerousNoAuth),
        );
        transport.set_inbound_handler(Arc::new(ElicitHandler));

        let result = transport
            .send_request("tools/call", Some(serde_json::json!({})))
            .await
            .expect("streaming request resolves once response frame arrives");
        server.abort();

        assert_eq!(result, serde_json::json!({ "ok": true }));
        assert!(
            ANSWER_SEEN.load(Ordering::SeqCst),
            "inbound answer must have been POSTed before the body closed"
        );
    }

    #[tokio::test]
    async fn http_sse_demux_dispatches_inbound_and_returns_response() {
        use std::convert::Infallible;

        use http_body_util::Full;
        use hyper::body::{Bytes, Incoming};
        use hyper::service::service_fn;
        use hyper::{Request, Response};
        use hyper_util::rt::TokioIo;

        // SSE body: a server-originated notification, then the response to
        // our outbound request (id 1).  The demux must surface the
        // notification to the handler and return {"ok":true} as the result.
        let sse = "event: message\n\
                   data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{\"level\":\"info\",\"data\":\"hi\"}}\n\n\
                   event: message\n\
                   data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept connection");
            let io = TokioIo::new(stream);
            let service = service_fn(move |_req: Request<Incoming>| async move {
                Ok::<_, Infallible>(
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Full::new(Bytes::from(sse)))
                        .expect("response"),
                )
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await;
        });

        let transport = HttpTransport::new(
            &format!("http://{addr}/mcp"),
            Box::new(crate::auth::DangerousNoAuth),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        transport.set_inbound_handler(Arc::new(RecordingHandler { notes: tx }));

        let result = transport
            .send_request("tools/call", Some(serde_json::json!({})))
            .await
            .expect("request should resolve to the response frame");
        server.abort();

        assert_eq!(result, serde_json::json!({ "ok": true }));
        let (method, params) = rx.try_recv().expect("inbound notification dispatched");
        assert_eq!(method, "notifications/message");
        assert_eq!(params.unwrap()["data"], "hi");
    }

    #[test]
    fn extract_last_sse_data_handles_single_frame() {
        // The shape Context7 (and most streamable HTTP MCP servers)
        // emit for initialize: one event with one data line, blank
        // line terminator.
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let extracted = extract_last_sse_data(body).unwrap();
        let v: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn extract_last_sse_data_takes_final_event() {
        // Defense for servers that ship a status frame before the
        // response — we want the LAST `data:`, not the first.
        let body = "event: message\ndata: {\"status\":\"in_progress\"}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":42}\n\n";
        let extracted = extract_last_sse_data(body).unwrap();
        let v: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(v["result"], 42);
    }

    #[test]
    fn extract_last_sse_data_concatenates_multiline_data_per_spec() {
        // SSE spec: multiple `data:` lines in one event get joined
        // with `\n`.  Real-world: rare for JSON payloads but valid
        // and we should not silently drop the second half.
        let body = "data: line one\ndata: line two\n\n";
        assert_eq!(extract_last_sse_data(body).unwrap(), "line one\nline two");
    }

    #[test]
    fn extract_last_sse_data_tolerates_missing_terminator() {
        // Some servers omit the trailing blank line on the final
        // event — we still need to surface that data.
        let body = "data: {\"a\":1}";
        assert_eq!(extract_last_sse_data(body).unwrap(), "{\"a\":1}");
    }

    #[test]
    fn extract_last_sse_data_returns_none_on_no_data_line() {
        // If the body is somehow empty or contains only `event:`
        // lines, we should return None so the caller surfaces a
        // useful error rather than panicking on parse.
        assert!(extract_last_sse_data("").is_none());
        assert!(extract_last_sse_data(":heartbeat\n\n").is_none());
    }

    #[tokio::test]
    async fn sequential_requests_match_by_id() {
        let transport = spawn_echo_server().await;

        let r1 = transport.send_request("method/one", None).await.unwrap();
        let r2 = transport.send_request("method/two", None).await.unwrap();

        assert_eq!(r1, serde_json::json!({"ok": true}));
        assert_eq!(r2, serde_json::json!({"ok": true}));
    }

    #[tokio::test]
    async fn request_timeout_on_dead_process() {
        // Spawn a process that immediately exits without responding.
        let transport = StdioTransport::spawn(
            "sh",
            &["-c".to_string(), "exit 0".to_string()],
            &HashMap::new(),
            false,
            false,
        )
        .await
        .expect("failed to spawn");

        // Give the process a moment to exit.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let err = transport.send_request("test", None).await;
        assert!(err.is_err(), "should fail when process dies");
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("closed") || msg.contains("timed out") || msg.contains("failed"),
            "error should indicate connection loss: {msg}"
        );
    }

    #[tokio::test]
    async fn rpc_error_is_propagated() {
        // Server returns a JSON-RPC error.
        let transport = StdioTransport::spawn(
            "sh",
            &[
                "-c".to_string(),
                r#"while IFS= read -r line; do
                    id=$(echo "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                    echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32601,\"message\":\"method not found\"}}"
                done"#
                    .to_string(),
            ],
            &HashMap::new(),
            false,
            false,
        )
        .await
        .expect("failed to spawn error server");

        let err = transport.send_request("nonexistent", None).await;
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("method not found"));
    }

    #[tokio::test]
    async fn drop_aborts_reader_task() {
        let transport = spawn_echo_server().await;
        // Just verify that dropping doesn't panic.
        drop(transport);
        // Give tokio a moment to clean up.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn spawn_nonexistent_command_fails() {
        let err = StdioTransport::spawn(
            "nonexistent-command-xyz",
            &[],
            &HashMap::new(),
            false,
            false,
        )
        .await;
        assert!(err.is_err());
    }

    // M7: when sandbox is required but the platform/PATH cannot
    // provide a wrapper (bwrap missing on Linux, no wrapper on
    // non-Linux), resolve_exec must refuse rather than silently
    // falling back to a direct unsandboxed spawn.
    #[test]
    fn resolve_exec_refuses_silent_fallback_when_bwrap_missing() {
        let err = StdioTransport::resolve_exec_with(
            "uvx", &["mcp-test".to_string()], true, false, false,
        );
        assert!(err.is_err(), "must refuse when sandbox requested and no bwrap");
        let msg = format!("{}", err.unwrap_err());
        assert!(
            msg.to_lowercase().contains("sandbox") || msg.to_lowercase().contains("bwrap"),
            "error must explain why: {msg}"
        );
    }

    #[test]
    fn resolve_exec_wraps_with_bwrap_when_available() {
        let (exec, argv) = StdioTransport::resolve_exec_with(
            "uvx", &["mcp-test".to_string()], true, true, false,
        )
        .expect("bwrap-available path");
        assert_eq!(exec, "bwrap");
        assert!(argv.iter().any(|a| a == "uvx"), "uvx must appear in argv");
    }

    #[test]
    fn resolve_exec_passthrough_when_sandbox_disabled() {
        let (exec, argv) = StdioTransport::resolve_exec_with(
            "uvx", &["mcp-test".to_string()], false, false, false,
        )
        .expect("passthrough path");
        assert_eq!(exec, "uvx");
        assert_eq!(argv, vec!["mcp-test".to_string()]);
    }
}
