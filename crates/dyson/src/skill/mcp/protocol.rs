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
    /// Optional execution metadata (MCP task augmentation).  When
    /// `execution.taskSupport == "required"`, the tool must be invoked as
    /// a task (`tools/call` with a `task` param, then poll `tasks/get` and
    /// fetch `tasks/result`) rather than awaited inline.
    #[serde(default)]
    pub execution: Option<McpToolExecution>,
}

impl McpToolDef {
    /// True when the server requires this tool to run as a task.
    pub fn requires_task(&self) -> bool {
        self.execution
            .as_ref()
            .and_then(|e| e.task_support.as_deref())
            == Some("required")
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct McpToolExecution {
    /// `"forbidden"` | `"optional"` | `"required"` (absent ⇒ forbidden).
    #[serde(rename = "taskSupport", default)]
    pub task_support: Option<String>,
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

/// Inner shape of an MCP `resource` content block — the spec's
/// `EmbeddedResource.resource`.  Implements both `BlobResourceContents`
/// (binary) and `TextResourceContents` (text) by accepting either
/// field; exactly one should be set per spec, but we tolerate both by
/// preferring `blob` when present.
///
/// Reference: <https://spec.modelcontextprotocol.io/specification/server/tools/#tool-result>
#[derive(Debug, Default, Deserialize)]
pub struct McpResourceContents {
    /// URI identifying the resource.  Spec-required.  We use only the
    /// trailing path component as the suggested filename, and that
    /// component goes through a sanitizer; any URI scheme is accepted.
    #[serde(default)]
    pub uri: String,
    /// Optional MIME type.  Defaults to `application/octet-stream`
    /// when absent (matches the dyson client's default for `image`
    /// content too).
    #[serde(
        rename = "mimeType",
        alias = "mime_type",
        default = "default_mcp_image_mime_type"
    )]
    pub mime_type: String,
    /// Spec `BlobResourceContents.blob` — base64-encoded body.  Empty
    /// when the resource carries `text` instead.
    #[serde(default)]
    pub blob: String,
    /// Spec `TextResourceContents.text` — UTF-8 body.  Empty when the
    /// resource carries `blob` instead.
    #[serde(default)]
    pub text: String,
}

fn default_mcp_image_mime_type() -> String {
    "application/octet-stream".to_string()
}

/// One entry in a `resources/list` response.  Only `uri` is spec-required;
/// `name`/`description`/`mimeType` are advisory metadata we surface to the
/// agent so it can decide what to read.
#[derive(Debug, Deserialize)]
pub struct McpResourceInfo {
    pub uri: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "mimeType", default)]
    pub mime_type: Option<String>,
}

/// A `resources/list` result.
#[derive(Debug, Default, Deserialize)]
pub struct McpResourcesListResult {
    #[serde(default)]
    pub resources: Vec<McpResourceInfo>,
}

/// A `resources/read` result.  Each entry reuses [`McpResourceContents`] —
/// the same `uri`/`mimeType`/`text`/`blob` shape carried by embedded
/// `resource` content blocks — so reads flow through `save_mcp_resource`.
#[derive(Debug, Default, Deserialize)]
pub struct McpResourcesReadResult {
    #[serde(default)]
    pub contents: Vec<McpResourceContents>,
}

/// One entry in a `prompts/list` response.
#[derive(Debug, Deserialize)]
pub struct McpPromptInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<McpPromptArgument>,
}

/// A declared argument for a prompt template.
#[derive(Debug, Deserialize)]
pub struct McpPromptArgument {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

/// A `prompts/list` result.
#[derive(Debug, Default, Deserialize)]
pub struct McpPromptsListResult {
    #[serde(default)]
    pub prompts: Vec<McpPromptInfo>,
}

/// A `prompts/get` result — a description plus the rendered conversation
/// messages the prompt expands to.  `content` is kept as raw JSON because
/// a prompt message carries a single content block (text/image/resource);
/// we render text inline and mark non-text blocks.
#[derive(Debug, Default, Deserialize)]
pub struct McpPromptGetResult {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub messages: Vec<McpPromptMessage>,
}

#[derive(Debug, Deserialize)]
pub struct McpPromptMessage {
    pub role: String,
    #[serde(default)]
    pub content: serde_json::Value,
}

/// Capabilities object returned by an MCP server in its `initialize`
/// response.  Every field is optional: a missing entry means the server
/// does not implement that primitive.  An empty object (`{}`) means the
/// server implements the primitive but offers no sub-feature flags.
///
/// We currently only act on `tools`; the others are stored so future
/// code can short-circuit calls to unimplemented primitives instead of
/// round-tripping a `-32601 Method not found` error.  Spec reference:
/// <https://spec.modelcontextprotocol.io/specification/basic/lifecycle/>
#[derive(Debug, Default, Deserialize, Clone)]
pub struct ServerCapabilities {
    #[serde(default)]
    pub tools: Option<ServerToolsCapability>,
    #[serde(default)]
    pub resources: Option<ServerResourcesCapability>,
    #[serde(default)]
    pub prompts: Option<ServerPromptsCapability>,
    #[serde(default)]
    pub logging: Option<serde_json::Value>,
    #[serde(default)]
    pub completions: Option<serde_json::Value>,
    /// MCP task augmentation.  When present, the server can run
    /// `taskSupport: required` tools as tasks; the client drives them via
    /// `tools/call` (+ `task` param) → `tasks/get` → `tasks/result`.
    #[serde(default)]
    pub tasks: Option<serde_json::Value>,
    #[serde(default)]
    pub experimental: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct ServerToolsCapability {
    #[serde(rename = "listChanged", default)]
    pub list_changed: bool,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct ServerResourcesCapability {
    #[serde(rename = "listChanged", default)]
    pub list_changed: bool,
    #[serde(default)]
    pub subscribe: bool,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct ServerPromptsCapability {
    #[serde(rename = "listChanged", default)]
    pub list_changed: bool,
}

/// Shape of the `initialize` response we parse on the client side.
/// We pull `capabilities` for future gating; `protocolVersion` and
/// `serverInfo` are accepted but unused today.
#[derive(Debug, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion", default)]
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: ServerCapabilities,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_result_parses_full_capabilities() {
        let raw = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": { "listChanged": true },
                "resources": { "listChanged": true, "subscribe": true },
                "prompts": { "listChanged": false },
                "logging": {},
                "completions": {},
                "experimental": { "x": 1 }
            },
            "serverInfo": { "name": "everything", "version": "1.0" }
        });
        let parsed: InitializeResult = serde_json::from_value(raw).expect("parse");
        assert_eq!(parsed.protocol_version, "2024-11-05");
        let caps = &parsed.capabilities;
        assert!(caps.tools.as_ref().unwrap().list_changed);
        let r = caps.resources.as_ref().unwrap();
        assert!(r.list_changed);
        assert!(r.subscribe);
        assert!(caps.prompts.is_some());
        assert!(caps.logging.is_some());
        assert!(caps.completions.is_some());
        assert!(caps.experimental.is_some());
    }

    #[test]
    fn initialize_result_handles_minimal_tools_only_server() {
        let raw = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "minimal", "version": "0.1" }
        });
        let parsed: InitializeResult = serde_json::from_value(raw).expect("parse");
        let caps = &parsed.capabilities;
        assert!(caps.tools.is_some());
        assert!(!caps.tools.as_ref().unwrap().list_changed);
        assert!(caps.resources.is_none());
        assert!(caps.prompts.is_none());
        assert!(caps.logging.is_none());
    }

    #[test]
    fn initialize_result_tolerates_empty_capabilities_object() {
        let raw = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "serverInfo": { "name": "no-features", "version": "0.1" }
        });
        let parsed: InitializeResult = serde_json::from_value(raw).expect("parse");
        assert!(parsed.capabilities.tools.is_none());
    }
}
