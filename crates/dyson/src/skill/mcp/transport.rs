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
//
// Layout: this file holds the trait, size caps and shared helpers;
// `transport/stdio.rs` and `transport/http.rs` hold the two transports.
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

use dyson_core::error::{DysonError, Result};

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
// `#[async_trait]` emits a bare `#[must_use]` on boxed-future trait methods.
#[allow(clippy::double_must_use)]
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
// `#[async_trait]` emits a bare `#[must_use]` on boxed-future trait methods.
#[allow(clippy::double_must_use)]
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
    Request {
        id: Value,
        method: String,
        params: Option<Value>,
    },
    Notification {
        method: String,
        params: Option<Value>,
    },
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

mod http;
mod stdio;

// Facade: both transports stay reachable as `transport::{Stdio,Http}Transport`.
pub use http::HttpTransport;
pub use stdio::StdioTransport;

#[cfg(test)]
mod tests;
