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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
        let (exec, exec_args) = Self::resolve_exec(command, args, sandbox, deny_network);
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
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Background reader task.
        let pending_clone = Arc::clone(&pending);
        let command_name = command.to_string();
        let reader_handle = tokio::spawn(async move {
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
            _reader_handle: reader_handle,
        })
    }
}

impl StdioTransport {
    /// Decide whether to invoke the MCP command directly or wrap it in
    /// `bwrap`.  Returns `(exec, argv)` suitable for `Command::new` +
    /// `.args()`.
    ///
    /// Non-Linux, missing bwrap, or `sandbox == false` → direct invocation.
    fn resolve_exec(
        command: &str,
        args: &[String],
        sandbox: bool,
        deny_network: bool,
    ) -> (String, Vec<String>) {
        if !sandbox {
            return (command.to_string(), args.to_vec());
        }
        #[cfg(target_os = "linux")]
        {
            let has_bwrap = std::process::Command::new("sh")
                .arg("-c")
                .arg("command -v bwrap")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if has_bwrap {
                let argv =
                    crate::sandbox::os::build_bwrap_argv_for_mcp_stdio(command, args, deny_network);
                return ("bwrap".to_string(), argv);
            }
            tracing::warn!(
                command = command,
                "MCP stdio sandbox requested but bwrap not found on PATH \
                 — falling back to UNSANDBOXED spawn"
            );
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = deny_network;
            tracing::warn!(
                command = command,
                "MCP stdio sandbox requested but only supported on Linux \
                 — falling back to UNSANDBOXED spawn"
            );
        }
        (command.to_string(), args.to_vec())
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
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(json.as_bytes())
                .await
                .map_err(|e| DysonError::Mcp {
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

        /// Maximum time to wait for an MCP server to respond to a request.
        const MCP_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

        let response = match tokio::time::timeout(MCP_RESPONSE_TIMEOUT, rx).await {
            Ok(Ok(resp)) => resp,
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

        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(json.as_bytes())
            .await
            .map_err(|e| DysonError::Mcp {
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
            client: crate::http::client().clone(),
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
    /// Shared by the normal path and the 401 retry path.
    async fn parse_rpc_response(
        &self,
        response: reqwest::Response,
    ) -> crate::error::Result<serde_json::Value> {
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

        let is_sse = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| {
                ct.split(';')
                    .next()
                    .is_some_and(|m| m.trim().eq_ignore_ascii_case("text/event-stream"))
            });

        let body = response.text().await.map_err(|e| DysonError::Mcp {
            server: self.url.clone(),
            message: format!("failed to read response: {e}"),
        })?;

        let rpc_body: std::borrow::Cow<'_, str> = if is_sse {
            match extract_last_sse_data(&body) {
                Some(s) => std::borrow::Cow::Owned(s),
                None => {
                    return Err(DysonError::Mcp {
                        server: self.url.clone(),
                        message: "SSE response had no `data:` frame".into(),
                    });
                }
            }
        } else {
            std::borrow::Cow::Borrowed(body.as_str())
        };

        let rpc_response: JsonRpcResponse =
            serde_json::from_str(&rpc_body).map_err(|e| DysonError::Mcp {
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

/// Extract the last `data:` payload from a Server-Sent Events body.
///
/// Streamable HTTP MCP servers wrap the JSON-RPC response in an
/// `event: message\ndata: {...}\n\n` envelope (RFC-style SSE).  For
/// the request/response shape MCP uses for initialize / tools/list /
/// tools/call, exactly one frame is emitted — but defensively we
/// take the LAST `data:` line so future server behavior (a status
/// frame followed by the response, etc.) doesn't trip the parser.
fn extract_last_sse_data(body: &str) -> Option<String> {
    let mut last: Option<String> = None;
    let mut current: Option<String> = None;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            // Per the SSE spec the body starts after the colon and one
            // optional space.  Concatenate multi-line `data:` frames
            // with a `\n` separator (also per the spec).
            let chunk = rest.strip_prefix(' ').unwrap_or(rest);
            current = Some(match current.take() {
                Some(prev) => format!("{prev}\n{chunk}"),
                None => chunk.to_string(),
            });
        } else if line.is_empty() {
            // Blank line terminates an event.  Promote the current
            // accumulator to `last` and reset.
            if let Some(c) = current.take() {
                last = Some(c);
            }
        }
    }
    // Trailing event without a blank-line terminator (some servers
    // skip the terminator on the final frame).
    if let Some(c) = current {
        last = Some(c);
    }
    last
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

            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "failed to read body".into());
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

    #[tokio::test]
    async fn send_request_receives_matching_response() {
        let transport = spawn_echo_server().await;
        let result = transport.send_request("test/method", None).await.unwrap();
        assert_eq!(result, serde_json::json!({"ok": true}));
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
}
