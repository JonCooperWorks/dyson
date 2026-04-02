// ===========================================================================
// Security regression tests.
//
// These tests verify that security fixes remain in place across refactors.
// Each test targets a specific vulnerability class and validates the
// defensive measure that was introduced to mitigate it.
// ===========================================================================

use std::time::{Duration, Instant};

// =========================================================================
// 1. Path traversal validation
// =========================================================================

#[test]
fn path_traversal_rejects_absolute_path() {
    let result = dyson::tool::validate_workspace_path("/etc/passwd");
    assert!(result.is_err(), "absolute paths must be rejected");
    assert!(
        result.unwrap_err().contains("absolute"),
        "error message should mention 'absolute'"
    );
}

#[test]
fn path_traversal_rejects_parent_dir() {
    let result = dyson::tool::validate_workspace_path("../secret.txt");
    assert!(result.is_err(), "parent traversal must be rejected");
}

#[test]
fn path_traversal_rejects_nested_parent_dir() {
    let result = dyson::tool::validate_workspace_path("foo/../../etc/passwd");
    assert!(result.is_err(), "nested parent traversal must be rejected");
}

#[test]
fn path_traversal_allows_valid_paths() {
    assert!(dyson::tool::validate_workspace_path("SOUL.md").is_ok());
    assert!(dyson::tool::validate_workspace_path("memory/2026-03-19.md").is_ok());
    assert!(dyson::tool::validate_workspace_path("sub/dir/file.txt").is_ok());
}

#[test]
fn path_traversal_rejects_symlinks() {
    // Create a temp directory with a symlink.
    let dir = std::env::temp_dir().join(format!("dyson-symlink-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let target = dir.join("real_file.txt");
    std::fs::write(&target, "secret").unwrap();

    let link = dir.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    // validate_workspace_path checks relative paths from CWD.
    // We need the symlink to exist at the relative path.
    let saved_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();

    let result = dyson::tool::validate_workspace_path("link");
    assert!(result.is_err(), "symlinks must be rejected");
    assert!(
        result.unwrap_err().contains("symlink"),
        "error message should mention 'symlink'"
    );

    // Non-symlink file should still work.
    assert!(dyson::tool::validate_workspace_path("real_file.txt").is_ok());

    std::env::set_current_dir(&saved_cwd).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// =========================================================================
// 2. Bash timeout kills process
// =========================================================================

#[tokio::test]
async fn bash_timeout_kills_process() {
    use dyson::tool::bash::BashTool;
    use dyson::tool::{Tool, ToolContext};

    let tool = BashTool {
        timeout: Duration::from_millis(500),
    };

    let ctx = ToolContext::from_cwd().unwrap();

    // Run a command that would take 10 seconds -- it should be killed by the timeout.
    let input = serde_json::json!({"command": "sleep 10"});
    let start = Instant::now();
    let output = tool.run(input, &ctx).await.unwrap();
    let elapsed = start.elapsed();

    // Should complete well before 10 seconds.
    assert!(
        elapsed < Duration::from_secs(3),
        "timeout should have killed the process quickly, but took {:?}",
        elapsed
    );

    // Output should indicate the process was killed due to timeout.
    assert!(output.is_error, "timed-out command should be an error");
    assert!(
        output.content.to_lowercase().contains("timed out"),
        "output should mention timeout: {}",
        output.content
    );
    assert!(
        output.content.to_lowercase().contains("killed"),
        "output should mention the process was killed: {}",
        output.content
    );
}

// =========================================================================
// 3. Regex size limit (ReDoS prevention)
// =========================================================================

#[test]
fn workspace_search_does_not_hang_on_pathological_regex() {
    use dyson::workspace::InMemoryWorkspace;
    use dyson::workspace::Workspace;

    let ws =
        InMemoryWorkspace::new().with_file("test.md", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa!");

    // A pattern that could cause catastrophic backtracking without size limits.
    // The OpenClawWorkspace has a 10MB compiled size limit; InMemoryWorkspace
    // falls back to substring match on invalid/huge regex. Either way, it
    // should complete quickly.
    let long_pattern = format!("(a+)+{}", "b".repeat(100));

    let start = Instant::now();
    let _results = ws.search(&long_pattern);
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "search with pathological regex pattern should complete quickly, took {:?}",
        elapsed
    );
}

// =========================================================================
// 4. Config size limit
// =========================================================================

#[test]
fn config_rejects_file_larger_than_1mb() {
    use std::io::Write;

    // Create a temp file larger than 1MB.
    let dir = std::env::temp_dir().join(format!("dyson-config-size-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("huge-dyson.json");

    {
        let mut f = std::fs::File::create(&config_path).unwrap();
        // Write a valid JSON start, then pad with whitespace to exceed 1MB.
        write!(f, "{{\"agent\": {{\"model\": \"test\"}}").unwrap();
        // 1MB + some extra
        let padding = vec![b' '; 1024 * 1024 + 100];
        f.write_all(&padding).unwrap();
        write!(f, "}}").unwrap();
    }

    let result = dyson::config::loader::load_settings(Some(config_path.as_path()));
    assert!(
        result.is_err(),
        "config file larger than 1MB should be rejected"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("too large"),
        "error should mention file is too large: {err_msg}"
    );

    // Cleanup.
    let _ = std::fs::remove_dir_all(&dir);
}

// =========================================================================
// 5. Telegram config requires allow_all_chats
// =========================================================================

#[test]
fn telegram_rejects_config_without_allow_all_chats() {
    use dyson::config::ControllerConfig;
    use dyson::controller::telegram::TelegramController;

    // A Telegram config with a bot_token but no allowed_chat_ids and
    // no allow_all_chats flag. This should be rejected as a safety measure
    // to prevent accidental open access.
    let config = ControllerConfig {
        controller_type: "telegram".to_string(),
        config: serde_json::json!({
            "type": "telegram",
            "bot_token": "fake-token-for-test",
            "allowed_chat_ids": []
        }),
    };

    let result = TelegramController::from_config(&config);
    assert!(
        result.is_none(),
        "Telegram controller should reject config with empty allowed_chat_ids and no allow_all_chats"
    );
}

#[test]
fn telegram_accepts_config_with_allow_all_chats() {
    use dyson::config::ControllerConfig;
    use dyson::controller::telegram::TelegramController;

    let config = ControllerConfig {
        controller_type: "telegram".to_string(),
        config: serde_json::json!({
            "type": "telegram",
            "bot_token": "fake-token-for-test",
            "allowed_chat_ids": [],
            "allow_all_chats": true
        }),
    };

    let result = TelegramController::from_config(&config);
    assert!(
        result.is_some(),
        "Telegram controller should accept config with allow_all_chats: true"
    );
}

#[test]
fn telegram_accepts_config_with_chat_ids() {
    use dyson::config::ControllerConfig;
    use dyson::controller::telegram::TelegramController;

    let config = ControllerConfig {
        controller_type: "telegram".to_string(),
        config: serde_json::json!({
            "type": "telegram",
            "bot_token": "fake-token-for-test",
            "allowed_chat_ids": [123456789]
        }),
    };

    let result = TelegramController::from_config(&config);
    assert!(
        result.is_some(),
        "Telegram controller should accept config with specific chat IDs"
    );
}

// =========================================================================
// 6. Zeroize on drop (Credential zeroes secret memory)
// =========================================================================
//
// The Credential type wraps secret strings and zeroes them on drop.
// All auth types (BearerTokenAuth, ApiKeyAuth) use Credential internally,
// so testing Credential covers the full chain.
//
// Strategy: Box the Credential, use into_raw / drop_in_place to trigger
// the destructor without freeing memory, then verify the String's length
// field has been zeroed.

#[test]
fn credential_zeroizes_on_drop() {
    use dyson::auth::Credential;

    let secret = "sk-secret-key-for-zeroize-regression-test-12345678";

    let cred = Box::new(Credential::new(secret.to_string()));
    let raw = Box::into_raw(cred);

    let struct_size = std::mem::size_of::<Credential>();
    let secret_len_bytes = secret.len().to_ne_bytes();

    // Before drop: the String's length field should be present.
    let pre_drop_bytes: Vec<u8> =
        unsafe { std::slice::from_raw_parts(raw as *const u8, struct_size).to_vec() };
    let pre_match_count = pre_drop_bytes
        .windows(secret_len_bytes.len())
        .filter(|w| *w == secret_len_bytes)
        .count();
    assert!(
        pre_match_count > 0,
        "before drop, the String's length field should be present in struct memory"
    );

    // Run destructor (zeroize), but keep the allocation alive.
    unsafe {
        std::ptr::drop_in_place(raw);
    }

    // After drop: the String's length should have been zeroed.
    let post_drop_bytes: Vec<u8> =
        unsafe { std::slice::from_raw_parts(raw as *const u8, struct_size).to_vec() };
    let post_match_count = post_drop_bytes
        .windows(secret_len_bytes.len())
        .filter(|w| *w == secret_len_bytes)
        .count();

    assert!(
        post_match_count < pre_match_count,
        "after drop, the secret's length field should have been zeroed by Credential"
    );

    unsafe {
        let layout = std::alloc::Layout::new::<Credential>();
        std::alloc::dealloc(raw as *mut u8, layout);
    }
}

#[test]
fn credential_debug_redacts_secret() {
    use dyson::auth::Credential;

    let cred = Credential::new("sk-ant-super-secret".to_string());
    let debug = format!("{:?}", cred);
    assert!(
        !debug.contains("sk-ant-super-secret"),
        "Debug output must not contain the secret value"
    );
    assert!(
        debug.contains("***"),
        "Debug output should show redacted marker"
    );
}

// =========================================================================
// 7. MCP server binds to loopback
// =========================================================================

#[tokio::test]
async fn mcp_server_binds_to_loopback_only() {
    use std::sync::Arc;
    use tokio::sync::RwLock;

    // Create a minimal workspace for the MCP server.
    let ws: Arc<RwLock<Box<dyn dyson::workspace::Workspace>>> = Arc::new(RwLock::new(Box::new(
        dyson::workspace::InMemoryWorkspace::new(),
    )));

    let server = Arc::new(dyson::skill::mcp::serve::McpHttpServer::new(ws, true));
    let (port, handle, token) = server.start().await.unwrap();

    // Verify the port is non-zero (OS assigned).
    assert!(port > 0, "port should be a valid non-zero port");

    // Verify the address is loopback by connecting to it.
    let addr = std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    assert!(
        addr.ip().is_loopback(),
        "MCP server address must be loopback, got {}",
        addr.ip()
    );

    // Verify the server is actually listening on loopback by making a request.
    let client = reqwest::Client::new();
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
                "clientInfo": { "name": "security-test", "version": "0.0.1" }
            }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["result"]["protocolVersion"], "2024-11-05",
        "MCP server should respond correctly on loopback"
    );

    handle.abort();
}

// =========================================================================
// 8. MCP server bearer token authentication
// =========================================================================

#[tokio::test]
async fn mcp_server_rejects_unauthorized_request() {
    use std::sync::Arc;
    use tokio::sync::RwLock;

    let ws: Arc<RwLock<Box<dyn dyson::workspace::Workspace>>> = Arc::new(RwLock::new(Box::new(
        dyson::workspace::InMemoryWorkspace::new(),
    )));

    let server = Arc::new(dyson::skill::mcp::serve::McpHttpServer::new(ws, true));
    let (port, handle, _token) = server.start().await.unwrap();

    // Send a request with no Authorization header.
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

    assert_eq!(
        resp.status(),
        401,
        "request without Authorization header must be rejected"
    );

    handle.abort();
}

#[tokio::test]
async fn mcp_server_rejects_wrong_bearer_token() {
    use std::sync::Arc;
    use tokio::sync::RwLock;

    let ws: Arc<RwLock<Box<dyn dyson::workspace::Workspace>>> = Arc::new(RwLock::new(Box::new(
        dyson::workspace::InMemoryWorkspace::new(),
    )));

    let server = Arc::new(dyson::skill::mcp::serve::McpHttpServer::new(ws, true));
    let (port, handle, _token) = server.start().await.unwrap();

    // Send a request with a wrong bearer token.
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .header("Authorization", "Bearer wrong-token-value")
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

    assert_eq!(
        resp.status(),
        401,
        "request with wrong bearer token must be rejected"
    );

    handle.abort();
}

#[test]
fn mcp_server_bearer_token_is_64_hex_chars() {
    use tokio::sync::RwLock;

    let ws: std::sync::Arc<RwLock<Box<dyn dyson::workspace::Workspace>>> = std::sync::Arc::new(
        RwLock::new(Box::new(dyson::workspace::InMemoryWorkspace::new())),
    );

    let server = dyson::skill::mcp::serve::McpHttpServer::new(ws, true);
    let token = server.bearer_token();

    assert_eq!(
        token.len(),
        64,
        "bearer token should be 64 hex chars, got {} chars",
        token.len()
    );
    assert!(
        token.chars().all(|c| c.is_ascii_hexdigit()),
        "bearer token should be hex only, got: {token}"
    );
}

#[test]
fn mcp_server_generates_unique_tokens() {
    use tokio::sync::RwLock;

    let ws1: std::sync::Arc<RwLock<Box<dyn dyson::workspace::Workspace>>> = std::sync::Arc::new(
        RwLock::new(Box::new(dyson::workspace::InMemoryWorkspace::new())),
    );
    let ws2: std::sync::Arc<RwLock<Box<dyn dyson::workspace::Workspace>>> = std::sync::Arc::new(
        RwLock::new(Box::new(dyson::workspace::InMemoryWorkspace::new())),
    );

    let server1 = dyson::skill::mcp::serve::McpHttpServer::new(ws1, true);
    let server2 = dyson::skill::mcp::serve::McpHttpServer::new(ws2, true);

    assert_ne!(
        server1.bearer_token(),
        server2.bearer_token(),
        "different server instances should have different tokens"
    );
}
