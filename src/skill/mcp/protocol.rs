// ===========================================================================
// MCP JSON-RPC protocol — message types for the Model Context Protocol.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Defines the JSON-RPC request/response types used by MCP (Model Context
//   Protocol).  MCP is a simple protocol: JSON-RPC 2.0 over stdio or SSE.
//   There are only a handful of methods we need:
//
//   1. initialize    — handshake, negotiate capabilities
//   2. tools/list    — discover what tools the server provides
//   3. tools/call    — execute a tool and get the result
//
// JSON-RPC 2.0 refresher:
//   Every message has a "jsonrpc": "2.0" field.
//   Requests have: id, method, params
//   Responses have: id, result (or error)
//   Notifications have: method, params (no id, no response expected)
//
// MCP specifics:
//   - The client (Dyson) sends `initialize` first
//   - The server responds with its capabilities and tool list
//   - The client sends `initialized` notification (no response)
//   - Then tools/list, tools/call as needed
// ===========================================================================

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// JSON-RPC base types
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 request.
#[derive(Debug, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn new(id: u64, method: &str, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        }
    }
}

/// A JSON-RPC 2.0 notification (no id, no response expected).
#[derive(Debug, Serialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: &'static str,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcNotification {
    pub fn new(method: &str, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            method: method.to_string(),
            params,
        }
    }
}

/// A JSON-RPC 2.0 response (success or error).
#[derive(Debug, Deserialize, Serialize)]
pub struct JsonRpcResponse {
    pub id: Option<u64>,
    pub result: Option<serde_json::Value>,
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// MCP-specific types
// ---------------------------------------------------------------------------

/// Tool definition returned by `tools/list`.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct McpToolDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Option<serde_json::Value>,
}

/// Result of a `tools/call` invocation.
#[derive(Debug, Deserialize)]
pub struct McpToolResult {
    #[serde(default)]
    pub content: Vec<McpContent>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

/// Content block in a tool result.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum McpContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(other)]
    Unknown,
}
