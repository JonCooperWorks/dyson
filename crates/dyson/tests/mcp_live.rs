//! Live MCP client integration test against the official reference
//! "everything" server, which implements the full primitive set
//! (tools, resources, prompts, completion, logging, sampling).
//!
//! Ignored by default — it needs `npx` and network access to fetch
//! `@modelcontextprotocol/server-everything`.  Run explicitly with:
//!
//! ```bash
//! cargo test -p dyson --test mcp_live -- --ignored --nocapture
//! ```
//!
//! It exercises the bidirectional handshake end-to-end: spawn, capability
//! negotiation (the client advertises roots; the server advertises
//! resources/prompts/…), `tools/list`, and the capability-gated
//! registration of the `<server>_resources` / `<server>_prompts` tools.

use std::collections::HashMap;
use std::sync::Arc;

use dyson::config::{McpConfig, McpTransportConfig};
use dyson::skill::Skill;
use dyson::skill::mcp::McpSkill;
use dyson::tool::{Tool, ToolContext};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

fn everything_config() -> McpConfig {
    McpConfig {
        name: "everything".into(),
        transport: McpTransportConfig::Stdio {
            command: "npx".into(),
            args: vec![
                "-y".into(),
                "@modelcontextprotocol/server-everything".into(),
            ],
            env: HashMap::new(),
            sandbox: false,
            sandbox_deny_network: false,
        },
    }
}

#[tokio::test]
#[ignore = "needs npx + network: npx -y @modelcontextprotocol/server-everything"]
async fn everything_server_handshake_registers_capability_gated_tools() {
    let mut skill = McpSkill::new(everything_config());
    skill
        .on_load()
        .await
        .expect("MCP initialize + notifications/initialized + tools/list should succeed");

    let names: Vec<String> = skill.tools().iter().map(|t| t.name().to_string()).collect();
    eprintln!("discovered {} tools: {names:?}", names.len());
    eprintln!(
        "system prompt:\n{}",
        skill.system_prompt().unwrap_or("(none)")
    );

    // tools/list round-tripped over the bidi transport.
    assert!(!names.is_empty(), "expected the everything server's tools");

    // Capability negotiation parsed the server's advertised resources +
    // prompts capabilities and registered the gated access tools.
    assert!(
        names.iter().any(|n| n == "everything_resources"),
        "resources capability should register the resources tool; got {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "everything_prompts"),
        "prompts capability should register the prompts tool; got {names:?}"
    );
}

/// Minimal ToolContext for invoking a discovered MCP tool — the MCP tools
/// don't read any of these fields, but `run()` requires the struct.
fn bare_ctx() -> ToolContext {
    ToolContext {
        working_dir: std::env::current_dir().unwrap(),
        env: HashMap::new(),
        cancellation: CancellationToken::new(),
        workspace: None,
        depth: 0,
        sandbox_bypass: None,
        taint_indexes: Arc::new(RwLock::new(HashMap::new())),
        activity: None,
        tool_use_id: None,
        subagent_events: None,
        artefacts: None,
        current_chat_id: None,
    }
}

/// End-to-end test of the inbound (server → client) request path: the
/// everything server's `get-roots-list` tool asks the *client* for its
/// roots via a server-originated `roots/list` request.  Exercises the bidi
/// transport reader → InboundHandler → NotificationRouter → response
/// written back over stdin → server returns the roots in its tool result.
#[tokio::test]
#[ignore = "needs npx + network: npx -y @modelcontextprotocol/server-everything"]
async fn server_originated_roots_list_round_trips() {
    let mut skill = McpSkill::new(everything_config());
    skill.on_load().await.expect("handshake");

    let tool = skill
        .tools()
        .iter()
        .find(|t| t.name() == "get-roots-list")
        .expect("everything server exposes get-roots-list")
        .clone();

    let out = tool
        .run(&serde_json::json!({}), &bare_ctx())
        .await
        .expect("get-roots-list should run");

    eprintln!("get-roots-list result (is_error={}):\n{}", out.is_error, out.content);
    assert!(!out.is_error, "roots/list round-trip should not error: {}", out.content);
    // The router answers with `file://<cwd>`; the server echoes the roots
    // it received back into the tool result.
    assert!(
        out.content.contains("file://"),
        "expected a file:// root the client answered with, got: {}",
        out.content
    );
}
