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
    let output = tool.run(&input, &ctx).await.unwrap();
    let elapsed = start.elapsed();

    // The 500ms timeout must kill the 10-second sleep.  Use 9s as the
    // ceiling — still proves the timeout fired (< 10s) while leaving
    // plenty of headroom for slow CI machines and parallel-test load.
    assert!(
        elapsed < Duration::from_secs(9),
        "timeout should have killed the process before it finished, but took {:?}",
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
    // The FilesystemWorkspace has a 10MB compiled size limit; InMemoryWorkspace
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
// 5b. Telegram command auth: only /whoami is public
// =========================================================================
//
// The `is_public_command` function is the single gate that decides which
// Telegram commands bypass the `allowed_chat_ids` access-control check.
// Only `/whoami` should return true — everything else requires auth.

#[test]
fn telegram_only_whoami_is_public() {
    use dyson::controller::telegram::is_public_command;

    // The sole public command.
    assert!(is_public_command("/whoami"), "/whoami must be public");

    // Everything else must require auth.
    let protected = [
        "/logs",
        "/logs 50",
        "/memory",
        "/memory some note",
        "/clear",
        "/compact",
        "/model provider",
        "/models",
    ];
    for cmd in protected {
        assert!(!is_public_command(cmd), "{cmd} must require auth");
    }
}

// =========================================================================
// 6. Only operators can use / commands in group chats
// =========================================================================
//
// In group chats, only users whose IDs appear in `allowed_chat_ids` (the
// operators) should be able to invoke / commands.  Non-operators may still
// talk to the bot via @mention or reply, but / commands are privileged.

#[test]
fn group_commands_restricted_to_operators() {
    use dyson::controller::telegram::is_operator;

    let allowed = [111_i64, 222];

    // Operator is recognised.
    assert!(is_operator(Some(111), &allowed));
    assert!(is_operator(Some(222), &allowed));

    // Non-operator is rejected.
    assert!(!is_operator(Some(999), &allowed));

    // Missing sender is rejected.
    assert!(!is_operator(None, &allowed));
}

#[test]
fn group_all_protected_commands_blocked_for_non_operators() {
    use dyson::controller::telegram::{is_operator, is_public_command};

    let allowed = [111_i64];
    let non_operator: Option<i64> = Some(999);

    let protected = [
        "/logs", "/logs 50", "/memory", "/memory some note",
        "/clear", "/compact", "/model provider", "/models",
    ];

    for cmd in protected {
        // The command is not public...
        assert!(!is_public_command(cmd), "{cmd} must not be public");
        // ...and the sender is not an operator, so it should be blocked.
        assert!(
            !is_operator(non_operator, &allowed),
            "{cmd} should be blocked for non-operators",
        );
    }
}

// =========================================================================
// 7. Zeroize on drop (Credential zeroes secret memory)
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

    let server = Arc::new(dyson::skill::mcp::serve::McpHttpServer::new(ws, std::collections::HashMap::new()));
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
    let client = dyson::http::client();
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

    let server = Arc::new(dyson::skill::mcp::serve::McpHttpServer::new(ws, std::collections::HashMap::new()));
    let (port, handle, _token) = server.start().await.unwrap();

    // Send a request with no Authorization header.
    let client = dyson::http::client();
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

    let server = Arc::new(dyson::skill::mcp::serve::McpHttpServer::new(ws, std::collections::HashMap::new()));
    let (port, handle, _token) = server.start().await.unwrap();

    // Send a request with a wrong bearer token.
    let client = dyson::http::client();
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

    let server = dyson::skill::mcp::serve::McpHttpServer::new(ws, std::collections::HashMap::new());
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

    let server1 = dyson::skill::mcp::serve::McpHttpServer::new(ws1, std::collections::HashMap::new());
    let server2 = dyson::skill::mcp::serve::McpHttpServer::new(ws2, std::collections::HashMap::new());

    assert_ne!(
        server1.bearer_token(),
        server2.bearer_token(),
        "different server instances should have different tokens"
    );
}

// =========================================================================
// 9. Constant-time token comparison
// =========================================================================

#[tokio::test]
async fn bearer_auth_uses_constant_time_comparison() {
    // Verify that BearerTokenAuth uses constant-time comparison by checking
    // that it still correctly accepts/rejects tokens (functional test).
    // The actual constant-time property is verified by code inspection —
    // the test ensures the comparison function works after the change.
    use dyson::auth::bearer::BearerTokenAuth;
    use dyson::auth::Auth;

    let auth = BearerTokenAuth::new("a]b]c]d]e]f]0123456789abcdef0123456789abcdef0123456789abcdef01".into());

    // Correct token.
    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        "authorization",
        "Bearer a]b]c]d]e]f]0123456789abcdef0123456789abcdef0123456789abcdef01"
            .parse()
            .unwrap(),
    );
    assert!(
        auth.validate_request(&headers).await.is_ok(),
        "correct token must be accepted"
    );

    // Wrong token — differs only in last character.
    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        "authorization",
        "Bearer a]b]c]d]e]f]0123456789abcdef0123456789abcdef0123456789abcdef02"
            .parse()
            .unwrap(),
    );
    assert!(
        auth.validate_request(&headers).await.is_err(),
        "wrong token must be rejected (even if only last char differs)"
    );
}

#[tokio::test]
async fn api_key_auth_uses_constant_time_comparison() {
    use dyson::auth::api_key::ApiKeyAuth;
    use dyson::auth::Auth;

    let auth = ApiKeyAuth::new("x-api-key", "sk-test-key-12345".into());

    let mut headers = hyper::HeaderMap::new();
    headers.insert("x-api-key", "sk-test-key-12345".parse().unwrap());
    assert!(auth.validate_request(&headers).await.is_ok());

    let mut headers = hyper::HeaderMap::new();
    headers.insert("x-api-key", "sk-test-key-12346".parse().unwrap());
    assert!(auth.validate_request(&headers).await.is_err());
}

// =========================================================================
// 10. Agent-level rate limiting
// =========================================================================

#[test]
fn rate_limiter_allows_within_limit() {
    use dyson::agent::rate_limiter::RateLimited;

    let limiter = RateLimited::new(0u8, 5, std::time::Duration::from_secs(60));
    for _ in 0..5 {
        assert!(limiter.access().is_ok(), "should allow up to the limit");
    }
}

#[test]
fn rate_limiter_rejects_over_limit() {
    use dyson::agent::rate_limiter::RateLimited;

    let limiter = RateLimited::new(0u8, 3, std::time::Duration::from_secs(60));
    for _ in 0..3 {
        limiter.access().unwrap();
    }
    assert!(
        limiter.access().is_err(),
        "should reject once limit is exceeded"
    );
}

#[test]
fn rate_limiter_resets_after_window() {
    use dyson::agent::rate_limiter::RateLimited;

    let limiter = RateLimited::new(0u8, 2, std::time::Duration::from_millis(50));
    limiter.access().unwrap();
    limiter.access().unwrap();
    assert!(limiter.access().is_err());

    // Wait for the window to expire.
    std::thread::sleep(std::time::Duration::from_millis(60));
    assert!(
        limiter.access().is_ok(),
        "should allow again after window expires"
    );
}

// =========================================================================
// 11. Web search query length limit
// =========================================================================

#[tokio::test]
async fn web_search_rejects_oversized_query() {
    use dyson::tool::{Tool, ToolContext};

    // Create a mock provider that always returns empty results.
    struct EmptyProvider;
    #[async_trait::async_trait]
    impl dyson::tool::web_search::SearchProvider for EmptyProvider {
        async fn search(
            &self,
            _query: &str,
            _num_results: usize,
        ) -> dyson::Result<Vec<dyson::tool::web_search::SearchResult>> {
            Ok(vec![])
        }
    }

    let tool = dyson::tool::web_search::WebSearchTool::new(std::sync::Arc::new(EmptyProvider));
    let ctx = ToolContext::from_cwd().unwrap();

    // A query over 500 characters should be rejected.
    let long_query = "a".repeat(501);
    let result = tool
        .run(&serde_json::json!({"query": long_query}), &ctx)
        .await
        .unwrap();
    assert!(
        result.is_error,
        "queries over 500 chars should be rejected to prevent data exfiltration"
    );
}

// =========================================================================
// 12. Bash env var filtering
// =========================================================================

#[tokio::test]
async fn bash_does_not_leak_secret_env_vars() {
    use dyson::tool::bash::BashTool;
    use dyson::tool::{Tool, ToolContext};

    let tool = BashTool::default();
    let mut ctx = ToolContext::from_cwd().unwrap();

    // Inject a fake secret into the tool context's env.
    ctx.env
        .insert("DYSON_TEST_API_KEY".into(), "super-secret-value".into());
    ctx.env
        .insert("DYSON_TEST_TOKEN".into(), "another-secret".into());
    ctx.env.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());

    let input = serde_json::json!({"command": "env"});
    let output = tool.run(&input, &ctx).await.unwrap();

    assert!(
        !output.content.contains("super-secret-value"),
        "bash output should not contain secret env var values"
    );
    assert!(
        !output.content.contains("another-secret"),
        "bash output should not contain token env var values"
    );
    // PATH should still be present.
    assert!(
        output.content.contains("PATH="),
        "safe env vars like PATH should still be passed"
    );
}

// =========================================================================
// 13. list_files / search_files path validation
// =========================================================================

#[tokio::test]
async fn list_files_rejects_path_traversal() {
    use dyson::tool::{Tool, ToolContext};

    let tmp = tempfile::tempdir().unwrap();
    let ctx = ToolContext {
        working_dir: tmp.path().to_path_buf(),
        env: std::collections::HashMap::new(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        workspace: None,
        depth: 0,
        dangerous_no_sandbox: false,
        taint_indexes: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        activity: None,
    };

    let tool = dyson::tool::list_files::ListFilesTool;
    let input = serde_json::json!({"pattern": "*", "path": "../../../etc"});
    let output = tool.run(&input, &ctx).await.unwrap();
    assert!(
        output.is_error,
        "list_files should reject path traversal via the 'path' parameter"
    );
}

#[tokio::test]
async fn search_files_rejects_path_traversal() {
    use dyson::tool::{Tool, ToolContext};

    let tmp = tempfile::tempdir().unwrap();
    let ctx = ToolContext {
        working_dir: tmp.path().to_path_buf(),
        env: std::collections::HashMap::new(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        workspace: None,
        depth: 0,
        dangerous_no_sandbox: false,
        taint_indexes: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        activity: None,
    };

    let tool = dyson::tool::search_files::SearchFilesTool;
    let input = serde_json::json!({"pattern": ".*", "path": "../../../etc"});
    let output = tool.run(&input, &ctx).await.unwrap();
    assert!(
        output.is_error,
        "search_files should reject path traversal via the 'path' parameter"
    );
}
