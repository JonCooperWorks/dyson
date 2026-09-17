use super::artifacts::*;
use super::oauth_flow::extract_code;
use super::*;
use crate::tool::ToolContext;
use base64::Engine as _;

struct StaticTransport {
    result: serde_json::Value,
}

#[async_trait]
impl McpTransport for StaticTransport {
    async fn send_request(
        &self,
        method: &str,
        _params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        assert_eq!(method, "tools/call");
        Ok(self.result.clone())
    }

    async fn send_notification(
        &self,
        _method: &str,
        _params: Option<serde_json::Value>,
    ) -> Result<()> {
        Ok(())
    }
}

/// Transport that answers each method from a fixed map — lets a single
/// mock back both `resources/list` and `resources/read`.
struct ByMethodTransport {
    responses: std::collections::HashMap<String, serde_json::Value>,
}

#[async_trait]
impl McpTransport for ByMethodTransport {
    async fn send_request(
        &self,
        method: &str,
        _params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        self.responses
            .get(method)
            .cloned()
            .ok_or_else(|| DysonError::Mcp {
                server: "mock".into(),
                message: format!("unexpected method: {method}"),
            })
    }

    async fn send_notification(
        &self,
        _method: &str,
        _params: Option<serde_json::Value>,
    ) -> Result<()> {
        Ok(())
    }
}

fn resources_tool(responses: Vec<(&str, serde_json::Value)>) -> McpResourcesTool {
    let responses = responses
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    McpResourcesTool {
        tool_name: "ctx7_resources".to_string(),
        transport: Arc::new(ByMethodTransport { responses }),
        server_name: "ctx7".to_string(),
    }
}

#[tokio::test]
async fn resources_tool_list_formats_catalogue() {
    let tool = resources_tool(vec![(
        "resources/list",
        serde_json::json!({
            "resources": [
                { "uri": "file:///a.txt", "name": "a", "mimeType": "text/plain" },
                { "uri": "file:///b.bin" }
            ]
        }),
    )]);
    let tmp = tempfile::tempdir().unwrap();
    let out = tool
        .run(
            &serde_json::json!({ "op": "list" }),
            &ToolContext::for_test(tmp.path()),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("file:///a.txt"));
    assert!(out.content.contains("(a)"));
    assert!(out.content.contains("[text/plain]"));
    assert!(out.content.contains("file:///b.bin"));
}

#[tokio::test]
async fn resources_tool_read_saves_file_and_emits_marker() {
    let bytes = b"resource-bytes".to_vec();
    let tool = resources_tool(vec![(
        "resources/read",
        serde_json::json!({
            "contents": [{
                "uri": "file:///doc.txt",
                "mimeType": "text/plain",
                "blob": base64::engine::general_purpose::STANDARD.encode(&bytes)
            }]
        }),
    )]);
    let tmp = tempfile::tempdir().unwrap();
    let out = tool
        .run(
            &serde_json::json!({ "op": "read", "uri": "file:///doc.txt" }),
            &ToolContext::for_test(tmp.path()),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("doc.txt"));
    assert!(out.content.contains("text/plain"));
    // Binary (blob) bodies stay artefact-only — base64 round-trips
    // through the LLM waste tokens without helping.
    assert!(!out.content.contains("resource-bytes"));
    assert_eq!(out.files.len(), 1);
    assert_eq!(std::fs::read(&out.files[0]).unwrap(), bytes);
    let _ = std::fs::remove_file(&out.files[0]);
}

#[tokio::test]
async fn resources_tool_read_inlines_text_body_and_saves_file() {
    let body = "hello, this is the body";
    let tool = resources_tool(vec![(
        "resources/read",
        serde_json::json!({
            "contents": [{
                "uri": "file:///doc.txt",
                "mimeType": "text/plain",
                "text": body
            }]
        }),
    )]);
    let tmp = tempfile::tempdir().unwrap();
    let out = tool
        .run(
            &serde_json::json!({ "op": "read", "uri": "file:///doc.txt" }),
            &ToolContext::for_test(tmp.path()),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("doc.txt"));
    assert!(out.content.contains(body));
    assert_eq!(out.files.len(), 1);
    assert_eq!(std::fs::read(&out.files[0]).unwrap(), body.as_bytes());
    let _ = std::fs::remove_file(&out.files[0]);
}

#[tokio::test]
async fn resources_tool_read_truncates_oversized_text_body() {
    let body = "x".repeat(INLINE_TEXT_CAP * 2 + 7);
    let tool = resources_tool(vec![(
        "resources/read",
        serde_json::json!({
            "contents": [{
                "uri": "file:///big.txt",
                "mimeType": "text/plain",
                "text": body
            }]
        }),
    )]);
    let tmp = tempfile::tempdir().unwrap();
    let out = tool
        .run(
            &serde_json::json!({ "op": "read", "uri": "file:///big.txt" }),
            &ToolContext::for_test(tmp.path()),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("truncated"));
    assert!(out.content.len() < body.len() + 512);
    // Full body must still land in the artefact, untruncated.
    assert_eq!(std::fs::read(&out.files[0]).unwrap().len(), body.len());
    let _ = std::fs::remove_file(&out.files[0]);
}

#[tokio::test]
async fn floor_char_boundary_never_splits_multibyte() {
    // "héllo" — the é is 2 bytes (0xC3 0xA9).
    let s = "héllo";
    // Asking to truncate at byte 2 lands in the middle of é (boundary
    // is at 1 or 3); we must back off to 1.
    assert_eq!(floor_char_boundary(s, 2), 1);
    assert_eq!(floor_char_boundary(s, 100), s.len());
}

fn prompts_tool(responses: Vec<(&str, serde_json::Value)>) -> McpPromptsTool {
    let responses = responses
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    McpPromptsTool {
        tool_name: "srv_prompts".to_string(),
        transport: Arc::new(ByMethodTransport { responses }),
        server_name: "srv".to_string(),
    }
}

#[tokio::test]
async fn prompts_tool_list_shows_names_and_required_args() {
    let tool = prompts_tool(vec![(
        "prompts/list",
        serde_json::json!({
            "prompts": [
                { "name": "greet", "description": "say hi",
                  "arguments": [{ "name": "who", "required": true },
                                { "name": "lang", "required": false }] }
            ]
        }),
    )]);
    let tmp = tempfile::tempdir().unwrap();
    let out = tool
        .run(
            &serde_json::json!({ "op": "list" }),
            &ToolContext::for_test(tmp.path()),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("greet"));
    assert!(out.content.contains("say hi"));
    assert!(out.content.contains("who*")); // required marked
    assert!(out.content.contains("lang"));
}

#[tokio::test]
async fn prompts_tool_get_renders_messages() {
    let tool = prompts_tool(vec![(
        "prompts/get",
        serde_json::json!({
            "description": "a greeting",
            "messages": [
                { "role": "user", "content": { "type": "text", "text": "Hello there" } },
                { "role": "assistant", "content": { "type": "image", "data": "..." } }
            ]
        }),
    )]);
    let tmp = tempfile::tempdir().unwrap();
    let out = tool
        .run(
            &serde_json::json!({ "op": "get", "name": "greet" }),
            &ToolContext::for_test(tmp.path()),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("a greeting"));
    assert!(out.content.contains("[user] Hello there"));
    // Non-text block is marked, not dumped.
    assert!(out.content.contains("[image content block]"));
}

#[tokio::test]
async fn prompts_tool_get_requires_name() {
    let tool = prompts_tool(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let out = tool
        .run(
            &serde_json::json!({ "op": "get" }),
            &ToolContext::for_test(tmp.path()),
        )
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("name"));
}

#[tokio::test]
async fn resources_tool_read_requires_uri() {
    let tool = resources_tool(vec![]);
    let tmp = tempfile::tempdir().unwrap();
    let out = tool
        .run(
            &serde_json::json!({ "op": "read" }),
            &ToolContext::for_test(tmp.path()),
        )
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("uri"));
}

fn remote_tool(result: serde_json::Value) -> McpRemoteTool {
    McpRemoteTool {
        tool_name: "browser_screenshot".to_string(),
        description: "Take a screenshot".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        transport: Arc::new(StaticTransport { result }),
        server_name: "browser".to_string(),
        task_required: false,
    }
}

#[tokio::test]
async fn task_required_tool_drives_create_poll_result_lifecycle() {
    // Mock the three task methods: tools/call returns a task handle,
    // tasks/get reports completed, tasks/result returns the real
    // CallToolResult.  Proves run_as_task() walks the lifecycle and
    // decodes the final result rather than the task envelope.
    let responses = vec![
        (
            "tools/call",
            serde_json::json!({ "task": { "taskId": "t1", "status": "working", "pollInterval": 100 } }),
        ),
        ("tasks/get", serde_json::json!({ "taskId": "t1", "status": "completed" })),
        (
            "tasks/result",
            serde_json::json!({ "content": [{ "type": "text", "text": "research done" }], "isError": false }),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let tool = McpRemoteTool {
        tool_name: "simulate-research-query".to_string(),
        description: "task tool".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        transport: Arc::new(ByMethodTransport { responses }),
        server_name: "everything".to_string(),
        task_required: true,
    };
    let tmp = tempfile::tempdir().unwrap();
    let out = tool
        .run(
            &serde_json::json!({ "topic": "x" }),
            &ToolContext::for_test(tmp.path()),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert_eq!(out.content, "research done");
}

#[tokio::test]
async fn direct_mcp_text_result_over_100_kib_survives() {
    let payload = "x".repeat(128 * 1024);
    let tool = remote_tool(serde_json::json!({
        "content": [{ "type": "text", "text": payload.clone() }],
        "isError": false
    }));
    let tmp = tempfile::tempdir().unwrap();

    let output = tool
        .run(&serde_json::json!({}), &ToolContext::for_test(tmp.path()))
        .await
        .unwrap();

    assert_eq!(output.content, payload);
    assert_eq!(output.content.len(), 128 * 1024);
    assert_eq!(
        output
            .metadata
            .as_ref()
            .and_then(|m| m.get("dyson_output_kind"))
            .and_then(|v| v.as_str()),
        Some("mcp")
    );
}

#[tokio::test]
async fn mcp_image_content_block_is_not_dropped() {
    let image_bytes = b"fake png bytes for side channel".to_vec();
    let image_b64 = base64::engine::general_purpose::STANDARD.encode(&image_bytes);
    let tool = remote_tool(serde_json::json!({
        "content": [{
            "type": "image",
            "mimeType": "image/png",
            "data": image_b64.clone()
        }],
        "isError": false
    }));
    let tmp = tempfile::tempdir().unwrap();

    let output = tool
        .run(&serde_json::json!({}), &ToolContext::for_test(tmp.path()))
        .await
        .unwrap();

    assert_eq!(
        output.content,
        format!("[image: image/png, {} bytes]", image_bytes.len())
    );
    assert!(!output.content.contains(&image_b64));
    assert_eq!(output.files.len(), 1);
    assert_eq!(std::fs::read(&output.files[0]).unwrap(), image_bytes);
    let _ = std::fs::remove_file(&output.files[0]);
}

#[test]
fn extract_code_from_redirect_url() {
    let url = "http://127.0.0.1:9999/callback?code=abc123&state=xyz";
    assert_eq!(extract_code(url).as_deref(), Some("abc123"));
}

#[test]
fn extract_code_from_raw_code() {
    assert_eq!(extract_code("my-raw-code").as_deref(), Some("my-raw-code"));
}

#[test]
fn extract_code_from_url_with_other_params() {
    let url = "http://localhost/callback?state=s&code=the-code&extra=1";
    assert_eq!(extract_code(url).as_deref(), Some("the-code"));
}

#[test]
fn extract_code_empty_returns_none() {
    assert!(extract_code("").is_none());
}

#[test]
fn extract_code_url_encoded() {
    let url = "http://localhost/callback?code=a%20b&state=s";
    assert_eq!(extract_code(url).as_deref(), Some("a b"));
}

// ===========================================================================
// McpContent::Resource — artefact handling + adversarial tests
// ===========================================================================

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[tokio::test]
async fn mcp_resource_blob_variant_saves_as_file_and_emits_marker() {
    let bytes = b"hello-from-resource".to_vec();
    let tool = remote_tool(serde_json::json!({
        "content": [{
            "type": "resource",
            "resource": {
                "uri": "playwright-download://realdl.txt",
                "mimeType": "text/plain",
                "blob": b64(&bytes),
            }
        }],
        "isError": false
    }));
    let tmp = tempfile::tempdir().unwrap();

    let output = tool
        .run(&serde_json::json!({}), &ToolContext::for_test(tmp.path()))
        .await
        .unwrap();

    // Marker: short, predictable, no path in the prompt.
    assert_eq!(
        output.content,
        format!("[resource: realdl.txt, text/plain, {} bytes]", bytes.len())
    );
    // Base64 payload MUST NOT leak into the LLM-visible content.
    assert!(!output.content.contains(&b64(&bytes)));
    // File present on disk and has the right bytes.
    assert_eq!(output.files.len(), 1);
    assert_eq!(std::fs::read(&output.files[0]).unwrap(), bytes);
    // Filename preserves the original "realdl.txt" suffix.
    let basename = output.files[0]
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap();
    assert!(
        basename.ends_with("realdl.txt"),
        "filename should preserve original suffix: {basename}"
    );
    let _ = std::fs::remove_file(&output.files[0]);
}

#[tokio::test]
async fn mcp_resource_text_variant_saves_as_file_and_emits_marker() {
    // TextResourceContents — bytes live in `text`, not base64'd.
    let text = "hello-from-text-resource\n";
    let tool = remote_tool(serde_json::json!({
        "content": [{
            "type": "resource",
            "resource": {
                "uri": "playwright-download://config.toml",
                "mimeType": "text/plain",
                "text": text,
            }
        }],
        "isError": false
    }));
    let tmp = tempfile::tempdir().unwrap();

    let output = tool
        .run(&serde_json::json!({}), &ToolContext::for_test(tmp.path()))
        .await
        .unwrap();

    assert_eq!(
        output.content,
        format!("[resource: config.toml, text/plain, {} bytes]", text.len())
    );
    // The text body MUST NOT appear in the LLM-visible content
    // (it's an artefact, not inline prose).
    assert!(!output.content.contains(text));
    assert_eq!(output.files.len(), 1);
    assert_eq!(std::fs::read(&output.files[0]).unwrap(), text.as_bytes());
    let _ = std::fs::remove_file(&output.files[0]);
}

#[tokio::test]
async fn mcp_resource_prefers_blob_when_both_blob_and_text_present() {
    // Spec says exactly one should be set; tolerate both by
    // preferring blob (the binary path).
    let blob_bytes = b"binary-wins".to_vec();
    let tool = remote_tool(serde_json::json!({
        "content": [{
            "type": "resource",
            "resource": {
                "uri": "playwright-download://both.bin",
                "mimeType": "application/octet-stream",
                "blob": b64(&blob_bytes),
                "text": "this should be ignored",
            }
        }],
        "isError": false
    }));
    let tmp = tempfile::tempdir().unwrap();
    let output = tool
        .run(&serde_json::json!({}), &ToolContext::for_test(tmp.path()))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&output.files[0]).unwrap(), blob_bytes);
    let _ = std::fs::remove_file(&output.files[0]);
}

#[tokio::test]
async fn mcp_resource_rejects_no_body() {
    let tool = remote_tool(serde_json::json!({
        "content": [{
            "type": "resource",
            "resource": {
                "uri": "playwright-download://empty.bin",
                "mimeType": "application/octet-stream",
            }
        }],
        "isError": false
    }));
    let tmp = tempfile::tempdir().unwrap();
    let err = match tool
        .run(&serde_json::json!({}), &ToolContext::for_test(tmp.path()))
        .await
    {
        Ok(_) => panic!("expected resource validation to fail"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(msg.contains("neither blob nor text"), "got: {msg}");
}

#[tokio::test]
async fn mcp_resource_text_variant_rejects_oversize() {
    let big = "x".repeat(64 * 1024 * 1024 + 1);
    let tool = remote_tool(serde_json::json!({
        "content": [{
            "type": "resource",
            "resource": {
                "uri": "playwright-download://big.txt",
                "mimeType": "text/plain",
                "text": big,
            }
        }],
        "isError": false
    }));
    let tmp = tempfile::tempdir().unwrap();
    let err = match tool
        .run(&serde_json::json!({}), &ToolContext::for_test(tmp.path()))
        .await
    {
        Ok(_) => panic!("expected oversize text to fail"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(msg.contains("byte cap"), "got: {msg}");
}

#[tokio::test]
async fn mcp_resource_rejects_invalid_base64() {
    let tool = remote_tool(serde_json::json!({
        "content": [{
            "type": "resource",
            "resource": {
                "uri": "playwright-download://bad.bin",
                "mimeType": "application/octet-stream",
                "blob": "@@@not-base64@@@"
            }
        }],
        "isError": false
    }));
    let tmp = tempfile::tempdir().unwrap();
    let err = match tool
        .run(&serde_json::json!({}), &ToolContext::for_test(tmp.path()))
        .await
    {
        Ok(_) => panic!("expected resource validation to fail"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(msg.contains("invalid base64"), "got: {msg}");
}

#[tokio::test]
async fn mcp_resource_rejects_oversize_blob() {
    // 64 MiB + 1 byte, base64-encoded — fast to generate by repeating.
    // (We compare AFTER decode in save_mcp_resource.)
    let oversize = vec![b'A'; (64 * 1024 * 1024) + 1];
    let tool = remote_tool(serde_json::json!({
        "content": [{
            "type": "resource",
            "resource": {
                "uri": "playwright-download://big.bin",
                "mimeType": "application/octet-stream",
                "blob": b64(&oversize)
            }
        }],
        "isError": false
    }));
    let tmp = tempfile::tempdir().unwrap();
    let err = match tool
        .run(&serde_json::json!({}), &ToolContext::for_test(tmp.path()))
        .await
    {
        Ok(_) => panic!("expected resource validation to fail"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("64-byte cap") || msg.contains("byte cap"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn mcp_resource_with_path_traversal_uri_is_sanitized() {
    let bytes = b"contents".to_vec();
    // Malicious URI: ../../etc/shadow.  uri_basename returns "shadow";
    // safe_filename_part keeps it; the path that lands in /tmp is
    // /tmp/dyson_mcp_..._shadow — anchored under /tmp, NOT /etc.
    let tool = remote_tool(serde_json::json!({
        "content": [{
            "type": "resource",
            "resource": {
                "uri": "playwright-download://../../etc/shadow",
                "mimeType": "application/octet-stream",
                "blob": b64(&bytes)
            }
        }],
        "isError": false
    }));
    let tmp = tempfile::tempdir().unwrap();
    let output = tool
        .run(&serde_json::json!({}), &ToolContext::for_test(tmp.path()))
        .await
        .unwrap();
    let path = &output.files[0];
    assert!(
        path.starts_with(std::env::temp_dir()),
        "must land under /tmp, got {}",
        path.display()
    );
    assert!(
        !path.to_string_lossy().contains("/etc/"),
        "no /etc/ in path, got {}",
        path.display()
    );
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn mcp_resource_with_shell_meta_filename_is_sanitized() {
    let bytes = b"contents".to_vec();
    let tool = remote_tool(serde_json::json!({
        "content": [{
            "type": "resource",
            "resource": {
                "uri": "playwright-download://evil;rm -rf /.txt",
                "mimeType": "application/octet-stream",
                "blob": b64(&bytes)
            }
        }],
        "isError": false
    }));
    let tmp = tempfile::tempdir().unwrap();
    let output = tool
        .run(&serde_json::json!({}), &ToolContext::for_test(tmp.path()))
        .await
        .unwrap();
    let basename = output.files[0]
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap()
        .to_string();
    // Shell metas replaced with underscores.
    assert!(!basename.contains(';'));
    assert!(!basename.contains(' '));
    assert!(!basename.contains('/'));
    let _ = std::fs::remove_file(&output.files[0]);
}

#[tokio::test]
async fn mcp_resource_with_missing_uri_falls_back_to_resource() {
    let bytes = b"contents".to_vec();
    let tool = remote_tool(serde_json::json!({
        "content": [{
            "type": "resource",
            "resource": {
                "mimeType": "application/octet-stream",
                "blob": b64(&bytes)
            }
        }],
        "isError": false
    }));
    let tmp = tempfile::tempdir().unwrap();
    let output = tool
        .run(&serde_json::json!({}), &ToolContext::for_test(tmp.path()))
        .await
        .unwrap();
    // Empty uri → "" → uri_basename returns None → fallback "resource".
    assert!(output.content.contains("resource"));
    assert_eq!(output.files.len(), 1);
    let _ = std::fs::remove_file(&output.files[0]);
}

#[tokio::test]
async fn mcp_resource_with_missing_mime_defaults_to_octet_stream() {
    let bytes = b"contents".to_vec();
    let tool = remote_tool(serde_json::json!({
        "content": [{
            "type": "resource",
            "resource": {
                "uri": "playwright-download://x.bin",
                "blob": b64(&bytes)
            }
        }],
        "isError": false
    }));
    let tmp = tempfile::tempdir().unwrap();
    let output = tool
        .run(&serde_json::json!({}), &ToolContext::for_test(tmp.path()))
        .await
        .unwrap();
    assert!(output.content.contains("application/octet-stream"));
    let _ = std::fs::remove_file(&output.files[0]);
}

#[tokio::test]
async fn mcp_multiple_resource_blocks_get_distinct_paths() {
    // Two resources with the SAME name in one tool call must not
    // overwrite each other on disk; the per-block stamp+idx prefix
    // disambiguates.
    let bytes_a = b"a-payload".to_vec();
    let bytes_b = b"b-payload".to_vec();
    let tool = remote_tool(serde_json::json!({
        "content": [
            {
                "type": "resource",
                "resource": {
                    "uri": "playwright-download://same.txt",
                    "mimeType": "text/plain",
                    "blob": b64(&bytes_a)
                }
            },
            {
                "type": "resource",
                "resource": {
                    "uri": "playwright-download://same.txt",
                    "mimeType": "text/plain",
                    "blob": b64(&bytes_b)
                }
            }
        ],
        "isError": false
    }));
    let tmp = tempfile::tempdir().unwrap();
    let output = tool
        .run(&serde_json::json!({}), &ToolContext::for_test(tmp.path()))
        .await
        .unwrap();
    assert_eq!(output.files.len(), 2);
    assert_ne!(output.files[0], output.files[1], "paths must differ");
    assert_eq!(std::fs::read(&output.files[0]).unwrap(), bytes_a);
    assert_eq!(std::fs::read(&output.files[1]).unwrap(), bytes_b);
    let _ = std::fs::remove_file(&output.files[0]);
    let _ = std::fs::remove_file(&output.files[1]);
}

#[tokio::test]
async fn mcp_resource_alongside_text_preserves_both() {
    let bytes = b"contents".to_vec();
    let tool = remote_tool(serde_json::json!({
        "content": [
            { "type": "text", "text": "narration ahead of the file" },
            {
                "type": "resource",
                "resource": {
                    "uri": "playwright-download://realdl.txt",
                    "mimeType": "text/plain",
                    "blob": b64(&bytes)
                }
            }
        ],
        "isError": false
    }));
    let tmp = tempfile::tempdir().unwrap();
    let output = tool
        .run(&serde_json::json!({}), &ToolContext::for_test(tmp.path()))
        .await
        .unwrap();
    assert!(output.content.contains("narration ahead of the file"));
    assert!(output.content.contains("realdl.txt"));
    assert_eq!(output.files.len(), 1);
    let _ = std::fs::remove_file(&output.files[0]);
}

#[test]
fn uri_basename_extracts_last_path_component() {
    assert_eq!(
        uri_basename("playwright-download://realdl.txt"),
        Some("realdl.txt")
    );
    assert_eq!(
        uri_basename("https://example.com/files/foo.pdf"),
        Some("foo.pdf")
    );
    assert_eq!(uri_basename("opaque-no-scheme"), Some("opaque-no-scheme"));
    // Trailing slash → empty trailing segment → None.
    assert_eq!(uri_basename("https://example.com/files/"), None);
    // Empty input → None.
    assert_eq!(uri_basename(""), None);
    // Path traversal returns "shadow"; safe_filename_part later keeps it.
    assert_eq!(
        uri_basename("playwright-download://../../etc/shadow"),
        Some("shadow")
    );
}
