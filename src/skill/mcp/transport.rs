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
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};

use crate::error::{DysonError, Result};

use super::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};

// ---------------------------------------------------------------------------
// McpTransport trait
// ---------------------------------------------------------------------------

/// Abstract transport for MCP JSON-RPC communication.
///
/// Both stdio and HTTP implement this.  The McpSkill only sees this trait.
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
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    _child: Arc<Mutex<Child>>,
}

impl StdioTransport {
    /// Spawn an MCP server process and establish communication.
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
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
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Background reader task.
        let pending_clone = Arc::clone(&pending);
        let command_name = command.to_string();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                if line.is_empty() {
                    continue;
                }

                let response: JsonRpcResponse = match serde_json::from_str(&line) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::debug!(
                            server = command_name,
                            error = %e,
                            "failed to parse MCP server output"
                        );
                        continue;
                    }
                };

                if let Some(id) = response.id {
                    let mut pending = pending_clone.lock().await;
                    if let Some(tx) = pending.remove(&id) {
                        let _ = tx.send(response);
                    }
                }
            }

            tracing::debug!(server = command_name, "MCP server stdout closed");
        });

        Ok(Self {
            stdin,
            next_id: AtomicU64::new(1),
            pending,
            _child: Arc::new(Mutex::new(child)),
        })
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
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(json.as_bytes()).await.map_err(|e| DysonError::Mcp {
                server: method.to_string(),
                message: format!("stdin write failed: {e}"),
            })?;
            stdin.write_all(b"\n").await.map_err(|e| DysonError::Mcp {
                server: method.to_string(),
                message: format!("stdin write failed: {e}"),
            })?;
            stdin.flush().await.map_err(|e| DysonError::Mcp {
                server: method.to_string(),
                message: format!("stdin flush failed: {e}"),
            })?;
        }

        let response = rx.await.map_err(|_| DysonError::Mcp {
            server: method.to_string(),
            message: "response channel closed (server died?)".into(),
        })?;

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

        let mut stdin = self.stdin.lock().await;
        stdin.write_all(json.as_bytes()).await.map_err(|e| DysonError::Mcp {
            server: method.to_string(),
            message: format!("stdin write failed: {e}"),
        })?;
        stdin.write_all(b"\n").await.map_err(|e| DysonError::Mcp {
            server: method.to_string(),
            message: format!("stdin write failed: {e}"),
        })?;
        stdin.flush().await.map_err(|e| DysonError::Mcp {
            server: method.to_string(),
            message: format!("stdin flush failed: {e}"),
        })?;

        Ok(())
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
}

impl HttpTransport {
    /// Create a new HTTP transport.
    ///
    /// `url` is the MCP server endpoint.
    /// `auth` applies authentication headers to every request.
    pub fn new(url: &str, auth: Box<dyn crate::auth::Auth>) -> Self {
        Self {
            client: reqwest::Client::new(),
            url: url.to_string(),
            auth,
            next_id: AtomicU64::new(1),
            session_id: Mutex::new(None),
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

        let mut req = self.build_request(&json).await?;

        // Include session ID if we have one.
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

        // Capture session ID from response headers.
        if let Some(sid) = response.headers().get("mcp-session-id") {
            if let Ok(sid_str) = sid.to_str() {
                let mut session = self.session_id.lock().await;
                *session = Some(sid_str.to_string());
            }
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "failed to read body".into());
            return Err(DysonError::Mcp {
                server: self.url.clone(),
                message: format!("HTTP {status}: {body}"),
            });
        }

        let body = response.text().await.map_err(|e| DysonError::Mcp {
            server: self.url.clone(),
            message: format!("failed to read response: {e}"),
        })?;

        let rpc_response: JsonRpcResponse =
            serde_json::from_str(&body).map_err(|e| DysonError::Mcp {
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

    async fn send_notification(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<()> {
        let notification = JsonRpcNotification::new(method, params);
        let json = serde_json::to_string(&notification)?;

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
}
