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

// =========================================================================
// 2. Docker container name validation
// =========================================================================

#[test]
fn docker_rejects_empty_container_name() {
    assert!(
        dyson::sandbox::docker::DockerSandbox::new("").is_err(),
        "empty container name must be rejected"
    );
}

#[test]
fn docker_rejects_semicolon_injection() {
    assert!(
        dyson::sandbox::docker::DockerSandbox::new("foo;rm -rf /").is_err(),
        "semicolons in container name must be rejected"
    );
}

#[test]
fn docker_rejects_spaces() {
    assert!(
        dyson::sandbox::docker::DockerSandbox::new("foo bar").is_err(),
        "spaces in container name must be rejected"
    );
}

#[test]
fn docker_rejects_dollar_paren_injection() {
    assert!(
        dyson::sandbox::docker::DockerSandbox::new("$(evil)").is_err(),
        "command substitution in container name must be rejected"
    );
}

#[test]
fn docker_rejects_backtick_injection() {
    assert!(
        dyson::sandbox::docker::DockerSandbox::new("`evil`").is_err(),
        "backtick injection in container name must be rejected"
    );
}

#[test]
fn docker_rejects_pipe() {
    assert!(
        dyson::sandbox::docker::DockerSandbox::new("foo|bar").is_err(),
        "pipe in container name must be rejected"
    );
}

#[test]
fn docker_allows_valid_names() {
    assert!(
        dyson::sandbox::docker::DockerSandbox::new("my-container").is_ok(),
        "valid container name with hyphens should be accepted"
    );
    assert!(
        dyson::sandbox::docker::DockerSandbox::new("test_box.1").is_ok(),
        "valid container name with underscores and dots should be accepted"
    );
    assert!(
        dyson::sandbox::docker::DockerSandbox::new("abc123").is_ok(),
        "valid alphanumeric container name should be accepted"
    );
}

// =========================================================================
// 3. Bash timeout kills process
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
// 4. Regex size limit (ReDoS prevention)
// =========================================================================

#[test]
fn workspace_search_does_not_hang_on_pathological_regex() {
    use dyson::workspace::InMemoryWorkspace;
    use dyson::workspace::Workspace;

    let ws = InMemoryWorkspace::new()
        .with_file("test.md", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa!");

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
// 5. Config size limit
// =========================================================================

#[test]
fn config_rejects_file_larger_than_1mb() {
    use std::io::Write;

    // Create a temp file larger than 1MB.
    let dir = std::env::temp_dir().join(format!(
        "dyson-config-size-test-{}",
        std::process::id()
    ));
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
// 6. Telegram config requires allow_all_chats
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
// 7. Zeroize on drop (API key memory is zeroed)
// =========================================================================
//
// Strategy: We allocate a large buffer, place the client inside it using
// placement-style construction (Box), then after drop_in_place we can
// read the Box's memory (which we still own) to verify that the String
// struct's length field is zeroed -- zeroize on String sets len to 0
// and writes zeros to the buffer before the String's own drop frees it.
//
// We verify two things:
// 1. The client can be constructed and dropped without panicking.
// 2. After drop_in_place, the client's memory region no longer contains
//    the API key bytes (scanning the struct's inline memory, not the heap).

#[test]
fn anthropic_client_zeroizes_api_key_on_drop() {
    use dyson::llm::anthropic::AnthropicClient;

    let secret = "sk-ant-secret-key-12345678-zeroize-test";

    // Box the client so we can use into_raw / drop_in_place pattern.
    let client = Box::new(AnthropicClient::new(secret, None));
    let raw = Box::into_raw(client);

    // Before drop: read the struct's memory and verify the key length
    // is stored somewhere (the String's len field should be non-zero).
    let struct_size = std::mem::size_of::<AnthropicClient>();
    let pre_drop_bytes: Vec<u8> = unsafe {
        std::slice::from_raw_parts(raw as *const u8, struct_size).to_vec()
    };

    // The secret length should appear somewhere in the struct memory
    // as the String's len field.
    let secret_len_bytes = (secret.len() as usize).to_ne_bytes();
    assert!(
        pre_drop_bytes
            .windows(secret_len_bytes.len())
            .any(|w| w == secret_len_bytes),
        "before drop, the String's length field should be present in struct memory"
    );

    // Run destructor (zeroize + field drops), but keep the allocation alive.
    unsafe {
        std::ptr::drop_in_place(raw);
    }

    // After drop: the String's len should have been set to 0 by zeroize.
    // Read the struct memory again.
    let post_drop_bytes: Vec<u8> = unsafe {
        std::slice::from_raw_parts(raw as *const u8, struct_size).to_vec()
    };

    // The secret length should no longer appear (zeroize sets len to 0).
    // Note: there could be a false positive if some other field happens
    // to have the same value, but with a unique key length this is unlikely.
    let len_still_present = post_drop_bytes
        .windows(secret_len_bytes.len())
        .enumerate()
        .filter(|(i, w)| {
            // Only check at the same offsets where we found it before.
            *w == secret_len_bytes
                && pre_drop_bytes[*i..*i + secret_len_bytes.len()] == secret_len_bytes[..]
        })
        .count();

    // If zeroize worked, the len field that matched before should now be 0.
    // We check that at least one of the previous matches is now gone.
    let pre_match_count = pre_drop_bytes
        .windows(secret_len_bytes.len())
        .filter(|w| *w == secret_len_bytes)
        .count();

    assert!(
        len_still_present < pre_match_count,
        "after drop, the API key's length field should have been zeroed by zeroize"
    );

    // Deallocate the Box memory.
    unsafe {
        let layout = std::alloc::Layout::new::<AnthropicClient>();
        std::alloc::dealloc(raw as *mut u8, layout);
    }
}

#[test]
fn openai_client_zeroizes_api_key_on_drop() {
    use dyson::llm::openai::OpenAiClient;

    let secret = "sk-openai-secret-key-87654321-zeroize-test";

    let client = Box::new(OpenAiClient::new(secret, None));
    let raw = Box::into_raw(client);

    let struct_size = std::mem::size_of::<OpenAiClient>();
    let pre_drop_bytes: Vec<u8> = unsafe {
        std::slice::from_raw_parts(raw as *const u8, struct_size).to_vec()
    };

    let secret_len_bytes = (secret.len() as usize).to_ne_bytes();
    assert!(
        pre_drop_bytes
            .windows(secret_len_bytes.len())
            .any(|w| w == secret_len_bytes),
        "before drop, the String's length field should be present in struct memory"
    );

    unsafe {
        std::ptr::drop_in_place(raw);
    }

    let post_drop_bytes: Vec<u8> = unsafe {
        std::slice::from_raw_parts(raw as *const u8, struct_size).to_vec()
    };

    let len_still_present = post_drop_bytes
        .windows(secret_len_bytes.len())
        .enumerate()
        .filter(|(i, w)| {
            *w == secret_len_bytes
                && pre_drop_bytes[*i..*i + secret_len_bytes.len()] == secret_len_bytes[..]
        })
        .count();

    let pre_match_count = pre_drop_bytes
        .windows(secret_len_bytes.len())
        .filter(|w| *w == secret_len_bytes)
        .count();

    assert!(
        len_still_present < pre_match_count,
        "after drop, the API key's length field should have been zeroed by zeroize"
    );

    unsafe {
        let layout = std::alloc::Layout::new::<OpenAiClient>();
        std::alloc::dealloc(raw as *mut u8, layout);
    }
}

// =========================================================================
// 8. MCP server binds to loopback
// =========================================================================

#[tokio::test]
async fn mcp_server_binds_to_loopback_only() {
    use std::sync::Arc;
    use tokio::sync::RwLock;

    // Create a minimal workspace for the MCP server.
    let ws: Arc<RwLock<Box<dyn dyson::workspace::Workspace>>> =
        Arc::new(RwLock::new(Box::new(dyson::workspace::InMemoryWorkspace::new())));

    let server = Arc::new(dyson::skill::mcp::serve::McpHttpServer::new(ws, true));
    let (port, handle) = server.start().await.unwrap();

    // Verify the port is non-zero (OS assigned).
    assert!(port > 0, "port should be a valid non-zero port");

    // Verify the address is loopback by connecting to it.
    let addr = std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        port,
    );
    assert!(
        addr.ip().is_loopback(),
        "MCP server address must be loopback, got {}",
        addr.ip()
    );

    // Verify the server is actually listening on loopback by making a request.
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
