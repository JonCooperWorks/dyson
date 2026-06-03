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
//
// Dual usage — client AND server:
//   These types are shared between Dyson's MCP client (mod.rs, connecting
//   to external MCP servers) and Dyson's MCP server (serve.rs, exposing
//   workspace tools to Claude Code).  This is why some types derive both
//   `Deserialize` and `Serialize`:
//
//   - JsonRpcRequest, JsonRpcNotification: Serialize only (client builds)
//   - JsonRpcResponse, JsonRpcError: Deserialize + Serialize (client
//     parses them; server produces them)
//   - McpToolDef: Deserialize + Serialize (client parses from external
//     server; our server serializes for Claude Code)
//   - McpToolResult, McpContent: Deserialize only (client parses from
//     external server; our server builds raw JSON instead)
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
///
/// Used in two contexts:
/// - **Client side** (mod.rs): Deserialized from MCP server responses
///   when Dyson connects to external MCP servers.
/// - **Server side** (serve.rs): Serialized to produce JSON responses
///   when Dyson acts as an MCP server for Claude Code.
///
/// Both `Deserialize` and `Serialize` are needed because the same type
/// is used on both sides of the MCP protocol.
#[derive(Debug, Deserialize, Serialize)]
pub struct JsonRpcResponse {
    pub id: Option<u64>,
    pub result: Option<serde_json::Value>,
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object, nested inside `JsonRpcResponse.error`.
///
/// Standard error codes:
/// - `-32700`: Parse error (invalid JSON)
/// - `-32601`: Method not found
/// - `-32602`: Invalid params
/// - `-32603`: Internal error
///
/// Derives both `Deserialize` (for parsing remote MCP server errors) and
/// `Serialize` (for producing error responses in the MCP HTTP server).
#[derive(Debug, Deserialize, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// MCP-specific types
// ---------------------------------------------------------------------------

/// Tool definition as exchanged in `tools/list` responses.
///
/// Used in two contexts:
/// - **Client side** (mod.rs): Deserialized from external MCP server
///   `tools/list` responses to discover remote tools.
/// - **Server side** (serve.rs): Serialized into the MCP HTTP server's
///   `tools/list` response to advertise workspace tools to Claude Code.
///
/// The `Serialize` derive was added alongside the MCP HTTP server to
/// support the server-side use case.  Previously only `Deserialize`
/// was needed (Dyson was MCP-client-only).
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

impl JsonRpcResponse {
    /// Build a success response with an arbitrary JSON result.
    pub const fn success(id: Option<u64>, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build a JSON-RPC error response (no result, just error).
    pub fn rpc_error(id: Option<u64>, code: i64, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    /// Build an MCP tool result (content array + isError flag).
    pub fn tool_result(id: Option<u64>, text: impl Into<String>, is_error: bool) -> Self {
        Self {
            id,
            result: Some(serde_json::json!({
                "content": [{ "type": "text", "text": text.into() }],
                "isError": is_error
            })),
            error: None,
        }
    }
}

/// Content block in a tool result.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum McpContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(
            rename = "mimeType",
            alias = "mime_type",
            default = "default_mcp_image_mime_type"
        )]
        mime_type: String,
    },
    /// MCP-spec `resource` block — used by upstreams (and the swarm
    /// proxy's PlaywrightAssist) to deliver arbitrary file bytes
    /// alongside a tool response.  We treat it as an artefact: decode
    /// the base64 `blob`, write to /tmp preserving the suggested name
    /// where safe, surface the path through ToolOutput.files so the
    /// controller can deliver it to the user, and tell the LLM the
    /// name + size + MIME via a short marker.
    #[serde(rename = "resource")]
    Resource {
        #[serde(default)]
        resource: McpResourceContents,
    },
    #[serde(other)]
    Unknown,
}

/// Inner shape of a `resource` content block.  The MCP spec puts the
/// fields under `resource: { ... }` — we mirror that.  Either `blob`
/// (base64) or `text` may carry the body; we accept both shapes for
/// forward compatibility but the bytes-as-artefact path only fires
/// when `blob` is present.
#[derive(Debug, Default, Deserialize)]
pub struct McpResourceContents {
    /// URI identifying the resource.  May be a synthetic scheme like
    /// `playwright-download://realdl.txt` or any RFC3986-shaped uri;
    /// only the trailing path component is used as the artefact name,
    /// and even that goes through a sanitizer.
    #[serde(default)]
    pub uri: String,
    /// Optional MIME type — surfaced in the LLM-visible marker for
    /// context.  Defaults to `application/octet-stream` if absent.
    #[serde(
        rename = "mimeType",
        alias = "mime_type",
        default = "default_mcp_image_mime_type"
    )]
    pub mime_type: String,
    /// Base64-encoded body.  Empty when the resource is delivered as
    /// `text` instead — we don't currently consume the text variant.
    #[serde(default)]
    pub blob: String,
}

fn default_mcp_image_mime_type() -> String {
    "application/octet-stream".to_string()
}
