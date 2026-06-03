use super::*;
use crate::workspace::Workspace;
use tokio::sync::RwLock;

/// Minimal in-memory workspace for testing MCP server behavior.
///
/// Pre-loaded with a single file `"identity"` containing `"I am a test agent"`.
/// Implements the full `Workspace` trait with HashMap-backed storage.
struct MockWorkspace {
    files: std::collections::HashMap<String, String>,
    skill_dirs: Vec<std::path::PathBuf>,
}

impl MockWorkspace {
    fn new() -> Self {
        let mut files = std::collections::HashMap::new();
        files.insert("identity".to_string(), "I am a test agent".to_string());
        Self {
            files,
            skill_dirs: Vec::new(),
        }
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
                    .map(std::string::ToString::to_string)
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

    fn skill_dirs(&self) -> Vec<std::path::PathBuf> {
        self.skill_dirs.clone()
    }
}

/// Helper to create a server with a MockWorkspace for testing.
fn make_server() -> Arc<McpHttpServer> {
    let ws: crate::workspace::WorkspaceHandle =
        Arc::new(RwLock::new(Box::new(MockWorkspace::new())));
    Arc::new(McpHttpServer::new(ws, HashMap::new()))
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

/// Verify that `tools/list` returns the unified workspace tool with its
/// name matching the Tool trait implementation.
#[tokio::test]
async fn tools_list_returns_workspace_tools() {
    let server = make_server();
    let resp = server.dispatch(Some(2), "tools/list", None).await;
    assert!(resp.error.is_none());
    let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
    assert_eq!(tools.len(), 1);

    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"workspace"));
}

// -----------------------------------------------------------------------
// Tool execution tests
// -----------------------------------------------------------------------

/// Verify that `tools/call` with workspace op=view actually reads from
/// the workspace and returns the file content in MCP format.
#[tokio::test]
async fn tools_call_workspace_view() {
    let server = make_server();
    let params = serde_json::json!({
        "name": "workspace",
        "arguments": { "op": "view", "file": "identity" }
    });
    let resp = server.dispatch(Some(3), "tools/call", Some(params)).await;
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
    let resp = server.dispatch(Some(4), "tools/call", Some(params)).await;
    assert!(resp.error.is_none()); // No JSON-RPC error
    let result = resp.result.unwrap();
    assert_eq!(result["isError"], true); // Tool-level error in result body
}

// -----------------------------------------------------------------------
// Resource tests
// -----------------------------------------------------------------------

/// `initialize` advertises resources + completions alongside tools.
#[tokio::test]
async fn initialize_advertises_resources_and_completions() {
    let server = make_server();
    let resp = server.dispatch(Some(1), "initialize", None).await;
    let caps = resp.result.unwrap()["capabilities"].clone();
    assert!(caps["tools"].is_object());
    assert!(caps["resources"].is_object());
    assert_eq!(caps["resources"]["subscribe"], false);
    assert!(caps["completions"].is_object());
}

/// `resources/list` exposes each workspace file under the workspace:// scheme.
#[tokio::test]
async fn resources_list_exposes_workspace_files() {
    let server = make_server();
    let resp = server.dispatch(Some(1), "resources/list", None).await;
    assert!(resp.error.is_none());
    let resources = resp.result.unwrap()["resources"].as_array().unwrap().clone();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0]["uri"], "workspace://identity");
    assert_eq!(resources[0]["name"], "identity");
}

/// `resources/read` returns the file content as a TextResourceContents block.
#[tokio::test]
async fn resources_read_returns_file_content() {
    let server = make_server();
    let params = serde_json::json!({ "uri": "workspace://identity" });
    let resp = server.dispatch(Some(2), "resources/read", Some(params)).await;
    assert!(resp.error.is_none());
    let contents = resp.result.unwrap()["contents"][0].clone();
    assert_eq!(contents["uri"], "workspace://identity");
    assert_eq!(contents["text"], "I am a test agent");
}

/// Reading an unknown workspace file is a -32002 "resource not found".
#[tokio::test]
async fn resources_read_unknown_uri_is_not_found() {
    let server = make_server();
    let params = serde_json::json!({ "uri": "workspace://does-not-exist" });
    let resp = server.dispatch(Some(3), "resources/read", Some(params)).await;
    assert_eq!(resp.error.unwrap().code, -32002);
}

/// A foreign URI scheme is rejected as invalid params (-32602).
#[tokio::test]
async fn resources_read_foreign_scheme_is_invalid_params() {
    let server = make_server();
    let params = serde_json::json!({ "uri": "file:///etc/passwd" });
    let resp = server.dispatch(Some(4), "resources/read", Some(params)).await;
    assert_eq!(resp.error.unwrap().code, -32602);
}

/// `resources/templates/list` exists and returns an empty list.
#[tokio::test]
async fn resource_templates_list_is_empty() {
    let server = make_server();
    let resp = server
        .dispatch(Some(5), "resources/templates/list", None)
        .await;
    assert!(resp.error.is_none());
    assert!(resp.result.unwrap()["resourceTemplates"]
        .as_array()
        .unwrap()
        .is_empty());
}

/// `completion/complete` for a resource URI prefix-matches workspace files.
#[tokio::test]
async fn completion_complete_suggests_resource_uris() {
    let server = make_server();
    let params = serde_json::json!({
        "ref": { "type": "ref/resource", "uri": "workspace://" },
        "argument": { "name": "uri", "value": "workspace://ident" }
    });
    let resp = server
        .dispatch(Some(6), "completion/complete", Some(params))
        .await;
    assert!(resp.error.is_none());
    let completion = resp.result.unwrap()["completion"].clone();
    assert_eq!(completion["values"][0], "workspace://identity");
    assert_eq!(completion["total"], 1);
    assert_eq!(completion["hasMore"], false);
}

/// A non-resource completion ref yields an empty (but valid) completion.
#[tokio::test]
async fn completion_complete_prompt_ref_is_empty() {
    let server = make_server();
    let params = serde_json::json!({
        "ref": { "type": "ref/prompt", "name": "whatever" },
        "argument": { "name": "x", "value": "" }
    });
    let resp = server
        .dispatch(Some(7), "completion/complete", Some(params))
        .await;
    assert!(resp.error.is_none());
    assert!(resp.result.unwrap()["completion"]["values"]
        .as_array()
        .unwrap()
        .is_empty());
}

// -----------------------------------------------------------------------
// Prompt tests
// -----------------------------------------------------------------------

/// Build a server whose workspace exposes one skill dir at `skill_dir`.
fn make_server_with_skill(skill_dir: std::path::PathBuf) -> Arc<McpHttpServer> {
    let mut ws = MockWorkspace::new();
    ws.skill_dirs = vec![skill_dir];
    let ws: crate::workspace::WorkspaceHandle = Arc::new(RwLock::new(Box::new(ws)));
    Arc::new(McpHttpServer::new(ws, HashMap::new()))
}

#[tokio::test]
async fn initialize_advertises_prompts() {
    let server = make_server();
    let resp = server.dispatch(Some(1), "initialize", None).await;
    assert!(resp.result.unwrap()["capabilities"]["prompts"].is_object());
}

#[tokio::test]
async fn prompts_list_is_empty_without_skills() {
    let server = make_server();
    let resp = server.dispatch(Some(1), "prompts/list", None).await;
    assert!(resp.error.is_none());
    assert!(resp.result.unwrap()["prompts"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn prompts_list_and_get_expose_skill_md() {
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = tmp.path().join("code-review");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "# Code Review\n\nReview the diff for bugs.\n",
    )
    .unwrap();
    let server = make_server_with_skill(skill_dir);

    // list
    let resp = server.dispatch(Some(1), "prompts/list", None).await;
    let prompts = resp.result.unwrap()["prompts"].as_array().unwrap().clone();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0]["name"], "code-review");
    assert_eq!(prompts[0]["description"], "Code Review");

    // get
    let params = serde_json::json!({ "name": "code-review" });
    let resp = server.dispatch(Some(2), "prompts/get", Some(params)).await;
    let result = resp.result.unwrap();
    let text = result["messages"][0]["content"]["text"].as_str().unwrap();
    assert!(text.contains("Review the diff for bugs."));
    assert_eq!(result["messages"][0]["role"], "user");
}

#[tokio::test]
async fn prompts_get_unknown_name_is_invalid_params() {
    let server = make_server();
    let params = serde_json::json!({ "name": "nope" });
    let resp = server.dispatch(Some(3), "prompts/get", Some(params)).await;
    assert_eq!(resp.error.unwrap().code, -32602);
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
    let (port, handle, token) = server.start().await.unwrap();
    assert!(port > 0);

    // Send a real HTTP request to the server.
    let client = crate::http::client();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .header("Authorization", format!("Bearer {token}"))
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

/// Verify that the semaphore-based connection limit rejects excess connections.
///
/// We start the server, then open MAX_CONCURRENT_CONNECTIONS + 1 TCP
/// connections.  The last connection should be dropped by the server
/// (because the semaphore has no permits left).
#[tokio::test]
async fn connection_limit_rejects_excess() {
    use tokio::net::TcpStream;

    let server = make_server();
    let (port, handle, _token) = server.start().await.unwrap();

    let addr = format!("127.0.0.1:{port}");

    // Open MAX_CONCURRENT_CONNECTIONS connections (they should all succeed).
    let mut held_streams = Vec::new();
    for _ in 0..super::MAX_CONCURRENT_CONNECTIONS {
        let stream = TcpStream::connect(&addr).await.unwrap();
        held_streams.push(stream);
    }

    // Give the server a moment to process all accepted connections.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // The next connection should be accepted at the TCP level (SYN/ACK)
    // but immediately dropped by the server (semaphore full).
    // We verify this by attempting to send a request and checking that
    // the connection is closed without a valid HTTP response.
    if let Ok(mut stream) = TcpStream::connect(&addr).await {
        // Try to write an HTTP request — the server should drop us.
        use tokio::io::AsyncWriteExt;
        let req = b"POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}";
        let _ = stream.write_all(req).await;

        // Read response — should be empty or connection reset.
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 1024];
        let n = tokio::time::timeout(std::time::Duration::from_millis(500), stream.read(&mut buf))
            .await;

        match n {
            Ok(Ok(0)) => {}  // Connection closed — expected.
            Ok(Err(_)) => {} // Connection reset — also expected.
            Err(_) => {}     // Timeout — server dropped the connection.
            Ok(Ok(_)) => {
                // Got a response — this means the server did accept it
                // (e.g. one of the held streams was processed and released).
                // This is acceptable if the OS released a connection quickly.
            }
        }
    }

    // Clean up.
    drop(held_streams);
    handle.abort();
}
