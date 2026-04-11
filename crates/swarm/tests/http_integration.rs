//! End-to-end HTTP integration tests.
//!
//! Each test binds the hub to `127.0.0.1:0`, grabs the ephemeral port,
//! and talks to it with `reqwest`.  The hub runs in a background task
//! that's aborted at the end of the test.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use dyson_swarm_protocol::types::{
    HardwareInfo, NodeManifest, NodeStatus, Payload, SwarmResult, SwarmTask, TaskStatus,
};
use dyson_swarm_protocol::verify::{SwarmPublicKey, verify_signed_payload};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use swarm::{Hub, McpApiKey};
use swarm::http::build_router;
use swarm::key::HubKeyPair;

struct Harness {
    base_url: String,
    public_key_config: String,
    _task: tokio::task::JoinHandle<()>,
    _tempdir: tempfile::TempDir,
}

async fn start_hub() -> Harness {
    start_hub_with_api_key(None).await
}

fn sample_manifest(name: &str) -> NodeManifest {
    NodeManifest {
        node_name: name.into(),
        os: "linux".into(),
        hardware: HardwareInfo {
            cpus: vec![],
            gpus: vec![],
            ram_bytes: 16 * 1024 * 1024 * 1024,
            disk_free_bytes: 0,
        },
        capabilities: vec!["bash".into()],
        status: NodeStatus::Idle,
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

#[tokio::test]
async fn register_returns_node_id_and_token() {
    let h = start_hub().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/swarm/register", h.base_url))
        .json(&sample_manifest("alpha"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body["node_id"].as_str().unwrap().len() > 10);
    assert!(!body["token"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn sse_stream_yields_registered_then_heartbeat_ack() {
    let h = start_hub().await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap();

    let reg: Value = client
        .post(format!("{}/swarm/register", h.base_url))
        .json(&sample_manifest("beta"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = reg["token"].as_str().unwrap().to_string();

    // Open SSE.
    let resp = client
        .get(format!("{}/swarm/events", h.base_url))
        .bearer_auth(&token)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let mut stream = resp.bytes_stream();
    let mut buffer = Vec::new();

    // Read the initial `registered` event.
    let first = read_one_event(&mut stream, &mut buffer).await;
    assert!(first.contains("event: registered"));
    assert!(first.contains("\"node_id\""));

    // POST a heartbeat — should produce a heartbeat_ack event.
    let status = client
        .post(format!("{}/swarm/heartbeat", h.base_url))
        .bearer_auth(&token)
        .json(&NodeStatus::Idle)
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), 200);

    let second = read_one_event(&mut stream, &mut buffer).await;
    assert!(second.contains("event: heartbeat_ack"));
}

#[tokio::test]
async fn heartbeat_rejects_bad_token() {
    let h = start_hub().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/swarm/heartbeat", h.base_url))
        .bearer_auth("not-a-real-token")
        .json(&NodeStatus::Idle)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn blob_put_mismatch_is_rejected() {
    let h = start_hub().await;
    let client = reqwest::Client::new();

    // Register to get a token.
    let reg: Value = client
        .post(format!("{}/swarm/register", h.base_url))
        .json(&sample_manifest("blobtest"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = reg["token"].as_str().unwrap().to_string();

    let wrong_hash = "0".repeat(64);
    let resp = client
        .put(format!("{}/swarm/blob/{wrong_hash}", h.base_url))
        .bearer_auth(&token)
        .body("hello".to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn blob_put_then_get_roundtrip() {
    let h = start_hub().await;
    let client = reqwest::Client::new();

    let reg: Value = client
        .post(format!("{}/swarm/register", h.base_url))
        .json(&sample_manifest("blobtest"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = reg["token"].as_str().unwrap().to_string();

    let data = b"some bytes to store".to_vec();
    let hash = hex_sha256(&data);

    let put = client
        .put(format!("{}/swarm/blob/{hash}", h.base_url))
        .bearer_auth(&token)
        .body(data.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200);

    let got = client
        .get(format!("{}/swarm/blob/{hash}", h.base_url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200);
    let body = got.bytes().await.unwrap();
    assert_eq!(body.as_ref(), data.as_slice());
}

#[tokio::test]
async fn mcp_list_nodes_after_register() {
    let h = start_hub().await;
    let client = reqwest::Client::new();

    // Register the node we want to see in the list.
    let (_visible_id, _visible_token) = register_node(&client, &h.base_url, "visible").await;

    // Register a separate caller node whose token we use for the MCP call.
    // (list_nodes excludes the caller itself, so we need a different node.)
    let (_caller_id, caller_token) = register_node(&client, &h.base_url, "caller").await;

    let resp: Value = client
        .post(format!("{}/mcp", h.base_url))
        .bearer_auth(&caller_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "list_nodes", "arguments": {} }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("\"node_name\": \"visible\""));
}

#[tokio::test]
async fn mcp_dispatch_no_nodes_is_error() {
    let h = start_hub().await;
    let client = reqwest::Client::new();

    // Register a caller just to authenticate; there are no *other* nodes to dispatch to.
    // Mark it as Draining so it won't be selected by the constraint router.
    let (_caller_id, caller_token) = register_node(&client, &h.base_url, "caller").await;
    client
        .post(format!("{}/swarm/heartbeat", h.base_url))
        .bearer_auth(&caller_token)
        .json(&NodeStatus::Draining)
        .send()
        .await
        .unwrap();

    let resp: Value = client
        .post(format!("{}/mcp", h.base_url))
        .bearer_auth(&caller_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "swarm_dispatch",
                "arguments": {
                    "prompt": "do the thing",
                    "constraints": {}
                }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["result"]["isError"], Value::Bool(true));
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("no eligible node"));
}

#[tokio::test]
async fn mcp_dispatch_without_target_or_constraints_is_error() {
    let h = start_hub().await;
    let client = reqwest::Client::new();

    let (_caller_id, caller_token) = register_node(&client, &h.base_url, "caller").await;

    let resp: Value = client
        .post(format!("{}/mcp", h.base_url))
        .bearer_auth(&caller_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "swarm_dispatch",
                "arguments": { "prompt": "do the thing" }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["result"]["isError"], Value::Bool(true));
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("target_node_id") || text.contains("constraints"),
        "expected target/constraints hint in error, got: {text}"
    );
}

#[tokio::test]
async fn mcp_dispatch_with_unknown_target_node_id_is_error() {
    let h = start_hub().await;
    let client = reqwest::Client::new();

    let (_caller_id, caller_token) = register_node(&client, &h.base_url, "caller").await;

    let resp: Value = client
        .post(format!("{}/mcp", h.base_url))
        .bearer_auth(&caller_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "swarm_submit",
                "arguments": {
                    "prompt": "do the thing",
                    "target_node_id": "00000000-0000-0000-0000-000000000000"
                }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["result"]["isError"], Value::Bool(true));
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("target node not found"),
        "expected NodeNotFound message, got: {text}"
    );
}

#[tokio::test]
async fn mcp_list_nodes_exposes_full_hardware_and_heartbeat() {
    let h = start_hub().await;
    let client = reqwest::Client::new();

    // Register with a richer manifest so we can verify full CPU/GPU
    // detail comes through, plus disk_free_bytes and os.
    let manifest = NodeManifest {
        node_name: "rich".into(),
        os: "linux".into(),
        hardware: HardwareInfo {
            cpus: vec![dyson_swarm_protocol::types::CpuInfo {
                model: "Ryzen 9 7950X".into(),
                cores: 32,
                physical_cores: Some(16),
            }],
            gpus: vec![dyson_swarm_protocol::types::GpuInfo {
                model: "RTX 4090".into(),
                vram_bytes: 24 * 1024 * 1024 * 1024,
                driver: "560.35".into(),
                cores: None,
            }],
            ram_bytes: 128 * 1024 * 1024 * 1024,
            disk_free_bytes: 500 * 1024 * 1024 * 1024,
        },
        capabilities: vec!["bash".into()],
        status: NodeStatus::Idle,
    };
    client
        .post(format!("{}/swarm/register", h.base_url))
        .json(&manifest)
        .send()
        .await
        .unwrap();

    // Register a separate caller node so list_nodes doesn't self-exclude "rich".
    let (_caller_id, caller_token) = register_node(&client, &h.base_url, "caller").await;

    let resp: Value = client
        .post(format!("{}/mcp", h.base_url))
        .bearer_auth(&caller_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "list_nodes", "arguments": {} }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    let rows = parsed.as_array().unwrap();
    // "rich" should be in the list (caller is excluded, so we see rich only).
    assert!(rows.len() >= 1);
    let row = &rows[0];
    assert_eq!(row["node_name"], "rich");
    assert_eq!(row["os"], "linux");
    assert_eq!(row["status"], "idle");
    assert!(row["last_heartbeat_unix"].as_u64().unwrap() > 0);
    assert_eq!(row["hardware"]["disk_free_bytes"], 500u64 * 1024 * 1024 * 1024);
    assert_eq!(row["hardware"]["cpus"][0]["model"], "Ryzen 9 7950X");
    assert_eq!(row["hardware"]["cpus"][0]["cores"], 32);
    assert_eq!(row["hardware"]["gpus"][0]["model"], "RTX 4090");
    assert_eq!(
        row["hardware"]["gpus"][0]["vram_bytes"],
        24u64 * 1024 * 1024 * 1024
    );
    // Idle node should NOT carry busy_task_id.
    assert!(row.get("busy_task_id").is_none());
}

#[tokio::test]
async fn mcp_initialize_returns_protocol_version() {
    let h = start_hub().await;
    let client = reqwest::Client::new();

    let resp: Value = client
        .post(format!("{}/mcp", h.base_url))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(resp["id"], 1);
}

#[tokio::test]
async fn mcp_tools_list_has_expected_tools() {
    let h = start_hub().await;
    let client = reqwest::Client::new();

    let resp: Value = client
        .post(format!("{}/mcp", h.base_url))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let tools = resp["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "list_nodes",
        "swarm_status",
        "swarm_dispatch",
        "swarm_submit",
        "swarm_task_status",
        "swarm_task_checkpoints",
        "swarm_task_result",
        "swarm_task_cancel",
        "swarm_task_list",
    ] {
        assert!(names.contains(&expected), "missing tool: {expected}");
    }
}

/// End-to-end: register, dispatch via MCP, fake the node consuming the
/// SSE task, POST a result, assert the MCP caller gets it back.
#[tokio::test]
async fn end_to_end_dispatch_and_result() {
    let h = start_hub().await;
    let client = reqwest::Client::new();

    // 1. Register.
    let reg: Value = client
        .post(format!("{}/swarm/register", h.base_url))
        .json(&sample_manifest("worker"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = reg["token"].as_str().unwrap().to_string();

    // 2. Open SSE.
    let sse_client = reqwest::Client::new();
    let sse_resp = sse_client
        .get(format!("{}/swarm/events", h.base_url))
        .bearer_auth(&token)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(sse_resp.status(), 200);
    let mut stream = sse_resp.bytes_stream();
    let mut buffer = Vec::new();

    // Swallow the initial `registered` event.
    let first = read_one_event(&mut stream, &mut buffer).await;
    assert!(first.contains("event: registered"));

    // 3. Kick off swarm_dispatch from a separate task so the SSE parser
    //    and result POST can run concurrently.
    //    Use the worker's own token for MCP auth — this avoids registering
    //    a second idle node that could be picked by the constraint router.
    let base_url = h.base_url.clone();
    let dispatch_client = reqwest::Client::new();
    let dispatch_token = token.clone();
    let dispatch_task = tokio::spawn(async move {
        let resp: Value = dispatch_client
            .post(format!("{base_url}/mcp"))
            .bearer_auth(&dispatch_token)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "tools/call",
                "params": {
                    "name": "swarm_dispatch",
                    "arguments": {
                        "prompt": "compute 2 + 2",
                        "timeout_secs": 30,
                        "constraints": {}
                    }
                }
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        resp
    });

    // 4. Read the task event off the SSE stream.
    let task_event = tokio::time::timeout(
        Duration::from_secs(5),
        read_one_event(&mut stream, &mut buffer),
    )
    .await
    .expect("task event did not arrive");
    assert!(task_event.contains("event: task"));

    // Extract the base64 data line.
    let data_b64 = task_event
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .unwrap()
        .trim();
    let wire_bytes = STANDARD.decode(data_b64).unwrap();

    // Verify the signature with the protocol crate.
    let pk = SwarmPublicKey::from_config(&h.public_key_config).unwrap();
    let payload = verify_signed_payload(&wire_bytes, &pk).unwrap();
    let task: SwarmTask = serde_json::from_slice(payload).unwrap();
    assert_eq!(task.prompt, "compute 2 + 2");

    // 5. Post a result matching task_id.
    let result = SwarmResult {
        task_id: task.task_id.clone(),
        text: "4".into(),
        payloads: vec![],
        status: TaskStatus::Completed,
        duration_secs: 1,
    };
    let post = client
        .post(format!("{}/swarm/result", h.base_url))
        .bearer_auth(&token)
        .json(&result)
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 200);

    // 6. Dispatch call should return the result.
    let dispatch_response = tokio::time::timeout(Duration::from_secs(5), dispatch_task)
        .await
        .expect("dispatch did not return")
        .unwrap();
    let text = dispatch_response["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("\"text\": \"4\""));
    assert!(text.contains("completed"));
}

/// End-to-end: payload inline propagates from MCP dispatch through the
/// signed task to the SSE receiver.
#[tokio::test]
async fn dispatch_preserves_inline_payload() {
    let h = start_hub().await;
    let client = reqwest::Client::new();

    let reg: Value = client
        .post(format!("{}/swarm/register", h.base_url))
        .json(&sample_manifest("worker"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = reg["token"].as_str().unwrap().to_string();

    let sse_resp = client
        .get(format!("{}/swarm/events", h.base_url))
        .bearer_auth(&token)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    let mut stream = sse_resp.bytes_stream();
    let mut buffer = Vec::new();
    let _ = read_one_event(&mut stream, &mut buffer).await;

    let base_url = h.base_url.clone();
    let dispatch_client = reqwest::Client::new();
    let dispatch_token = token.clone();
    let dispatch_task = tokio::spawn(async move {
        dispatch_client
            .post(format!("{base_url}/mcp"))
            .bearer_auth(&dispatch_token)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "swarm_dispatch",
                    "arguments": {
                        "prompt": "read the file",
                        "payloads": [ Payload::Inline {
                            name: "config.yaml".into(),
                            data: b"key: value".to_vec(),
                        } ],
                        "timeout_secs": 30,
                        "constraints": {}
                    }
                }
            }))
            .send()
            .await
    });

    let task_event = tokio::time::timeout(
        Duration::from_secs(5),
        read_one_event(&mut stream, &mut buffer),
    )
    .await
    .expect("task event did not arrive");

    let data_b64 = task_event
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .unwrap()
        .trim();
    let wire_bytes = STANDARD.decode(data_b64).unwrap();
    let pk = SwarmPublicKey::from_config(&h.public_key_config).unwrap();
    let payload = verify_signed_payload(&wire_bytes, &pk).unwrap();
    let task: SwarmTask = serde_json::from_slice(payload).unwrap();

    assert_eq!(task.prompt, "read the file");
    assert_eq!(task.payloads.len(), 1);
    match &task.payloads[0] {
        Payload::Inline { name, data } => {
            assert_eq!(name, "config.yaml");
            assert_eq!(data, b"key: value");
        }
        _ => panic!("expected inline payload"),
    }

    // Close the dispatch task by posting a result.
    let result = SwarmResult {
        task_id: task.task_id.clone(),
        text: "ok".into(),
        payloads: vec![],
        status: TaskStatus::Completed,
        duration_secs: 0,
    };
    let _ = client
        .post(format!("{}/swarm/result", h.base_url))
        .bearer_auth(&token)
        .json(&result)
        .send()
        .await;
    let _ = dispatch_task.await;
}

/// Register a node and return `(node_id, token)`.
async fn register_node(client: &reqwest::Client, base_url: &str, name: &str) -> (String, String) {
    let reg: Value = client
        .post(format!("{base_url}/swarm/register"))
        .json(&sample_manifest(name))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        reg["node_id"].as_str().unwrap().to_string(),
        reg["token"].as_str().unwrap().to_string(),
    )
}

/// Call an MCP tool and return the parsed JSON payload from the
/// response's single text content block.  Collapses ~10 lines of
/// boilerplate into one call.
async fn mcp_call_tool(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    name: &str,
    arguments: Value,
) -> Value {
    let resp: Value = client
        .post(format!("{base_url}/mcp"))
        .bearer_auth(token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("tool result had no text content");
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

/// End-to-end: a task submitted via swarm_submit can be cancelled via
/// swarm_task_cancel.  The hub pushes a cancel_task SSE event to the
/// owning node and marks the record as Cancelled.
#[tokio::test]
async fn async_dispatch_can_be_cancelled() {
    let h = start_hub().await;
    let client = reqwest::Client::new();

    let reg: Value = client
        .post(format!("{}/swarm/register", h.base_url))
        .json(&sample_manifest("cancelee"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = reg["token"].as_str().unwrap().to_string();

    let sse_resp = client
        .get(format!("{}/swarm/events", h.base_url))
        .bearer_auth(&token)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    let mut stream = sse_resp.bytes_stream();
    let mut buffer = Vec::new();
    let _ = read_one_event(&mut stream, &mut buffer).await; // registered

    // Submit a task. Use the worker's own token for MCP auth to avoid
    // registering a second idle node that could be picked by the router.
    let submit = mcp_call_tool(
        &client,
        &h.base_url,
        &token,
        "swarm_submit",
        json!({ "prompt": "run forever", "constraints": {} }),
    )
    .await;
    let task_id = submit["task_id"].as_str().unwrap().to_string();

    // Drain the task SSE event.
    let _task_event = tokio::time::timeout(
        Duration::from_secs(5),
        read_one_event(&mut stream, &mut buffer),
    )
    .await
    .expect("task event did not arrive");

    // Cancel it.
    let cancel = mcp_call_tool(
        &client,
        &h.base_url,
        &token,
        "swarm_task_cancel",
        json!({ "task_id": task_id }),
    )
    .await;
    assert_eq!(cancel["state"], "cancelled");
    assert_eq!(cancel["event_delivered"], true);

    // Node should now see a cancel_task SSE event.
    let cancel_event = tokio::time::timeout(
        Duration::from_secs(5),
        read_one_event(&mut stream, &mut buffer),
    )
    .await
    .expect("cancel_task event did not arrive");
    assert!(cancel_event.contains("event: cancel_task"));
    assert!(cancel_event.contains(&task_id));

    // Status reflects cancelled.
    let status = mcp_call_tool(
        &client,
        &h.base_url,
        &token,
        "swarm_task_status",
        json!({ "task_id": task_id }),
    )
    .await;
    assert_eq!(status["state"]["state"], "cancelled");

    // Cancelling twice is an error.
    let resp: Value = client
        .post(format!("{}/mcp", h.base_url))
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "swarm_task_cancel",
                "arguments": { "task_id": task_id }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["result"]["isError"], true);

    // A late result POST from the node after cancellation does not
    // overwrite the Cancelled state (first-writer-wins in finalize).
    let late = SwarmResult {
        task_id: task_id.clone(),
        text: "finished just now".into(),
        payloads: vec![],
        status: TaskStatus::Completed,
        duration_secs: 5,
    };
    let post = client
        .post(format!("{}/swarm/result", h.base_url))
        .bearer_auth(&token)
        .json(&late)
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 200);

    let after = mcp_call_tool(
        &client,
        &h.base_url,
        &token,
        "swarm_task_status",
        json!({ "task_id": task_id }),
    )
    .await;
    assert_eq!(after["state"]["state"], "cancelled");
}

/// End-to-end: swarm_submit returns fast, node posts checkpoints, MCP
/// callers can poll status / checkpoints / result.
#[tokio::test]
async fn async_dispatch_with_checkpoints_end_to_end() {
    use dyson_swarm_protocol::types::TaskCheckpoint;

    let h = start_hub().await;
    let client = reqwest::Client::new();

    // Register a fake node and open its SSE stream.
    let reg: Value = client
        .post(format!("{}/swarm/register", h.base_url))
        .json(&sample_manifest("long-runner"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = reg["token"].as_str().unwrap().to_string();

    let sse_resp = client
        .get(format!("{}/swarm/events", h.base_url))
        .bearer_auth(&token)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(sse_resp.status(), 200);
    let mut stream = sse_resp.bytes_stream();
    let mut buffer = Vec::new();

    // Swallow the initial `registered` event.
    let first = read_one_event(&mut stream, &mut buffer).await;
    assert!(first.contains("event: registered"));

    // Call swarm_submit — it must return immediately with a task_id.
    // Use the worker's own token for MCP auth to avoid registering a
    // second idle node that could be picked by the constraint router.
    let submit_start = std::time::Instant::now();
    let submit_resp: Value = client
        .post(format!("{}/mcp", h.base_url))
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "swarm_submit",
                "arguments": {
                    "prompt": "fine tune a tiny model",
                    "constraints": {}
                }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let elapsed = submit_start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "swarm_submit should return fast, took {:?}",
        elapsed
    );

    let submit_text = submit_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let submit_payload: Value = serde_json::from_str(submit_text).unwrap();
    let task_id = submit_payload["task_id"].as_str().unwrap().to_string();
    assert!(!task_id.is_empty());
    assert_eq!(submit_payload["state"], "running");

    // The hub should have pushed a task event to our SSE stream.
    let task_event = tokio::time::timeout(
        Duration::from_secs(5),
        read_one_event(&mut stream, &mut buffer),
    )
    .await
    .expect("task event did not arrive");
    assert!(task_event.contains("event: task"));

    let data_b64 = task_event
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .unwrap()
        .trim();
    let wire_bytes = STANDARD.decode(data_b64).unwrap();
    let pk = SwarmPublicKey::from_config(&h.public_key_config).unwrap();
    let payload = verify_signed_payload(&wire_bytes, &pk).unwrap();
    let task: SwarmTask = serde_json::from_slice(payload).unwrap();
    assert_eq!(task.task_id, task_id);

    // POST two checkpoints as the node.
    for seq in 1..=2u32 {
        let cp = TaskCheckpoint {
            task_id: task_id.clone(),
            sequence: seq,
            message: format!("epoch {seq}"),
            progress: Some(seq as f32 * 0.3),
            emitted_at_secs: seq as u64 * 10,
        };
        let resp = client
            .post(format!("{}/swarm/checkpoint", h.base_url))
            .bearer_auth(&token)
            .json(&cp)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // swarm_task_status should now report 2 checkpoints, state=running.
    let status_resp: Value = client
        .post(format!("{}/mcp", h.base_url))
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "swarm_task_status",
                "arguments": { "task_id": task_id }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let status_text = status_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let status: Value = serde_json::from_str(status_text).unwrap();
    assert_eq!(status["checkpoint_count"], json!(2));
    assert_eq!(status["last_sequence"], json!(2));
    assert_eq!(status["state"]["state"], "running");

    // swarm_task_checkpoints with since_sequence=1 should return only #2.
    let cps_resp: Value = client
        .post(format!("{}/mcp", h.base_url))
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "swarm_task_checkpoints",
                "arguments": { "task_id": task_id, "since_sequence": 1 }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cps_text = cps_resp["result"]["content"][0]["text"].as_str().unwrap();
    let cps: Value = serde_json::from_str(cps_text).unwrap();
    assert_eq!(cps["checkpoints"].as_array().unwrap().len(), 1);
    assert_eq!(cps["checkpoints"][0]["sequence"], json!(2));

    // swarm_task_result while still running: state present, result absent.
    let pending_resp: Value = client
        .post(format!("{}/mcp", h.base_url))
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "swarm_task_result",
                "arguments": { "task_id": task_id }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pending_text = pending_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let pending: Value = serde_json::from_str(pending_text).unwrap();
    assert_eq!(pending["state"]["state"], "running");
    assert!(pending.get("result").is_none());

    // POST the final result.
    let final_result = SwarmResult {
        task_id: task_id.clone(),
        text: "done".into(),
        payloads: vec![],
        status: TaskStatus::Completed,
        duration_secs: 42,
    };
    let fin = client
        .post(format!("{}/swarm/result", h.base_url))
        .bearer_auth(&token)
        .json(&final_result)
        .send()
        .await
        .unwrap();
    assert_eq!(fin.status(), 200);

    // swarm_task_result now returns the full result and state=completed.
    let done_resp: Value = client
        .post(format!("{}/mcp", h.base_url))
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "swarm_task_result",
                "arguments": { "task_id": task_id }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let done_text = done_resp["result"]["content"][0]["text"].as_str().unwrap();
    let done: Value = serde_json::from_str(done_text).unwrap();
    assert_eq!(done["state"]["state"], "completed");
    assert_eq!(done["result"]["text"], "done");
    assert_eq!(done["result"]["duration_secs"], json!(42));

    // Late checkpoint after completion is rejected.
    let late_cp = TaskCheckpoint {
        task_id: task_id.clone(),
        sequence: 3,
        message: "too late".into(),
        progress: None,
        emitted_at_secs: 99,
    };
    let late_resp = client
        .post(format!("{}/swarm/checkpoint", h.base_url))
        .bearer_auth(&token)
        .json(&late_cp)
        .send()
        .await
        .unwrap();
    assert_eq!(late_resp.status(), 404);

    // swarm_task_list contains the task.
    let list_resp: Value = client
        .post(format!("{}/mcp", h.base_url))
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "swarm_task_list",
                "arguments": {}
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list_text = list_resp["result"]["content"][0]["text"].as_str().unwrap();
    let list: Value = serde_json::from_str(list_text).unwrap();
    assert!(list["count"].as_u64().unwrap() >= 1);
    let task_ids: Vec<&str> = list["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["task_id"].as_str())
        .collect();
    assert!(task_ids.contains(&task_id.as_str()));
}

/// Checkpoints with unknown task_id get a 404.
#[tokio::test]
async fn checkpoint_for_unknown_task_is_not_found() {
    use dyson_swarm_protocol::types::TaskCheckpoint;

    let h = start_hub().await;
    let client = reqwest::Client::new();

    let reg: Value = client
        .post(format!("{}/swarm/register", h.base_url))
        .json(&sample_manifest("probe"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = reg["token"].as_str().unwrap().to_string();

    let cp = TaskCheckpoint {
        task_id: "no-such-task".into(),
        sequence: 1,
        message: "should 404".into(),
        progress: None,
        emitted_at_secs: 0,
    };
    let resp = client
        .post(format!("{}/swarm/checkpoint", h.base_url))
        .bearer_auth(&token)
        .json(&cp)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// End-to-end: a caller picks a specific node by `target_node_id` and
/// the task is dispatched to that exact node (bypassing the constraint
/// router entirely).
#[tokio::test]
async fn explicit_target_node_id_routes_to_that_node() {
    let h = start_hub().await;
    let client = reqwest::Client::new();

    // Register two nodes. We'll only open an SSE stream for the second
    // one and target it explicitly; if the router were picking by
    // constraints it might route to either.
    let (_alpha_id, alpha_token) = register_node(&client, &h.base_url, "alpha").await;
    let beta: Value = client
        .post(format!("{}/swarm/register", h.base_url))
        .json(&sample_manifest("beta"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let beta_node_id = beta["node_id"].as_str().unwrap().to_string();
    let beta_token = beta["token"].as_str().unwrap().to_string();

    // Open SSE for beta only.
    let sse_resp = client
        .get(format!("{}/swarm/events", h.base_url))
        .bearer_auth(&beta_token)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    let mut stream = sse_resp.bytes_stream();
    let mut buffer = Vec::new();
    let _ = read_one_event(&mut stream, &mut buffer).await; // registered

    // Submit targeting beta (use alpha's token as the MCP caller).
    let submit = mcp_call_tool(
        &client,
        &h.base_url,
        &alpha_token,
        "swarm_submit",
        json!({
            "prompt": "compute",
            "target_node_id": beta_node_id,
        }),
    )
    .await;
    assert_eq!(submit["node_id"], beta_node_id);
    assert_eq!(submit["state"], "running");

    // beta's SSE stream should receive the task.
    let task_event = tokio::time::timeout(
        Duration::from_secs(5),
        read_one_event(&mut stream, &mut buffer),
    )
    .await
    .expect("beta did not receive task event");
    assert!(task_event.contains("event: task"));
}

/// End-to-end: once a node is busy with a task, submitting a second
/// task with the same `target_node_id` returns NodeNotIdle.
#[tokio::test]
async fn explicit_target_on_busy_node_is_error() {
    let h = start_hub().await;
    let client = reqwest::Client::new();

    let reg: Value = client
        .post(format!("{}/swarm/register", h.base_url))
        .json(&sample_manifest("solo"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let node_id = reg["node_id"].as_str().unwrap().to_string();
    let token = reg["token"].as_str().unwrap().to_string();

    // Open SSE so the hub can push the first task.
    let sse_resp = client
        .get(format!("{}/swarm/events", h.base_url))
        .bearer_auth(&token)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    let mut stream = sse_resp.bytes_stream();
    let mut buffer = Vec::new();
    let _ = read_one_event(&mut stream, &mut buffer).await; // registered

    // Register a separate caller node for MCP authentication.
    let (_caller_id, caller_token) = register_node(&client, &h.base_url, "caller").await;

    // Submit a first task — flips the node to Busy.
    let first = mcp_call_tool(
        &client,
        &h.base_url,
        &caller_token,
        "swarm_submit",
        json!({
            "prompt": "first",
            "target_node_id": node_id,
        }),
    )
    .await;
    assert_eq!(first["state"], "running");

    // Drain the task event so the hub's push path has settled.
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        read_one_event(&mut stream, &mut buffer),
    )
    .await
    .expect("first task event did not arrive");

    // Try a second submission targeting the same node — should fail
    // with NodeNotIdle.
    let resp: Value = client
        .post(format!("{}/mcp", h.base_url))
        .bearer_auth(&caller_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "swarm_submit",
                "arguments": {
                    "prompt": "second",
                    "target_node_id": node_id,
                }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["result"]["isError"], Value::Bool(true));
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("not idle") && text.contains("busy"),
        "expected NodeNotIdle/busy message, got: {text}"
    );
}

/// Pull one complete SSE event out of a byte stream.
async fn read_one_event(
    stream: &mut (impl StreamExt<Item = reqwest::Result<bytes::Bytes>> + Unpin),
    buffer: &mut Vec<u8>,
) -> String {
    loop {
        if let Some(pos) = find_subseq(buffer, b"\n\n") {
            let event = String::from_utf8_lossy(&buffer[..pos]).into_owned();
            buffer.drain(..pos + 2);
            return event;
        }
        match stream.next().await {
            Some(Ok(chunk)) => buffer.extend_from_slice(&chunk),
            Some(Err(e)) => panic!("SSE stream error: {e}"),
            None => panic!("SSE stream ended before event delivered"),
        }
    }
}

fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Static API key auth tests
// ---------------------------------------------------------------------------

/// Hash a plaintext API key for use in tests.
fn test_hash(plaintext: &str) -> String {
    use argon2::password_hash::SaltString;
    use argon2::{Argon2, PasswordHasher};
    let salt = SaltString::from_b64("dGVzdHNhbHR0ZXN0c2FsdA").unwrap();
    Argon2::default()
        .hash_password(plaintext.as_bytes(), &salt)
        .unwrap()
        .to_string()
}

async fn start_hub_with_api_key(hash: Option<&str>) -> Harness {
    let tempdir = tempfile::tempdir().unwrap();
    let key = HubKeyPair::generate(&tempdir.path().join("hub.key")).unwrap();
    let public_key_config = key.public_key_config();

    let api_key = hash.map(|h| McpApiKey::new(h.to_string()).unwrap());
    let hub = Hub::new(key, tempdir.path(), api_key).await.unwrap();
    let app = build_router(Arc::clone(&hub));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");

    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Harness {
        base_url,
        public_key_config,
        _task: task,
        _tempdir: tempdir,
    }
}

/// Raw MCP request that returns the full JSON-RPC response (not just the
/// tool result).  Useful for asserting error envelopes.
async fn mcp_raw(
    client: &reqwest::Client,
    base_url: &str,
    token: Option<&str>,
    method: &str,
    params: Option<Value>,
) -> Value {
    let mut body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
    });
    if let Some(p) = params {
        body["params"] = p;
    }
    let mut req = client.post(format!("{base_url}/mcp"));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    req.json(&body).send().await.unwrap().json().await.unwrap()
}

#[tokio::test]
async fn mcp_api_key_auth_accepts_valid_key() {
    let plaintext = "test-secret-key-12345";
    let hash = test_hash(plaintext);
    let h = start_hub_with_api_key(Some(&hash)).await;
    let client = reqwest::Client::new();

    let resp = mcp_raw(
        &client,
        &h.base_url,
        Some(plaintext),
        "tools/call",
        Some(json!({ "name": "swarm_status", "arguments": {} })),
    )
    .await;

    assert!(
        resp.get("result").is_some(),
        "expected success result, got: {resp}"
    );
    assert!(resp.get("error").is_none(), "unexpected error: {resp}");
}

#[tokio::test]
async fn mcp_api_key_auth_rejects_wrong_key() {
    let hash = test_hash("correct-key");
    let h = start_hub_with_api_key(Some(&hash)).await;
    let client = reqwest::Client::new();

    let resp = mcp_raw(
        &client,
        &h.base_url,
        Some("wrong-key"),
        "tools/call",
        Some(json!({ "name": "swarm_status", "arguments": {} })),
    )
    .await;

    assert!(
        resp.get("error").is_some(),
        "expected error for wrong key, got: {resp}"
    );
    let msg = resp["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("unauthorized"), "expected unauthorized, got: {msg}");
}

#[tokio::test]
async fn mcp_api_key_auth_rejects_no_auth() {
    let hash = test_hash("some-key");
    let h = start_hub_with_api_key(Some(&hash)).await;
    let client = reqwest::Client::new();

    let resp = mcp_raw(
        &client,
        &h.base_url,
        None,
        "tools/call",
        Some(json!({ "name": "swarm_status", "arguments": {} })),
    )
    .await;

    assert!(
        resp.get("error").is_some(),
        "expected error for no auth, got: {resp}"
    );
}

#[tokio::test]
async fn mcp_api_key_tasks_scoped_to_owner() {
    let api_key = "api-secret";
    let hash = test_hash(api_key);
    let h = start_hub_with_api_key(Some(&hash)).await;
    let client = reqwest::Client::new();

    // Register a node so there's a worker to accept tasks.
    let (_node_id, node_token) = register_node(&client, &h.base_url, "worker").await;

    // Open SSE stream so the node is considered alive and idle.
    let sse_resp = client
        .get(format!("{}/swarm/events", h.base_url))
        .bearer_auth(&node_token)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    let mut stream = sse_resp.bytes_stream();
    let mut buffer = Vec::new();
    let _ = read_one_event(&mut stream, &mut buffer).await; // registered

    // API key caller submits a task.
    let submit_resp = mcp_raw(
        &client,
        &h.base_url,
        Some(api_key),
        "tools/call",
        Some(json!({ "name": "swarm_submit", "arguments": { "prompt": "api task", "constraints": {} } })),
    )
    .await;
    assert!(
        submit_resp.get("result").is_some(),
        "api key submit failed: {submit_resp}"
    );

    // API key caller lists tasks — should see its own.
    let api_list = mcp_call_tool(
        &client,
        &h.base_url,
        api_key,
        "swarm_task_list",
        json!({}),
    )
    .await;
    let api_tasks = api_list["tasks"].as_array().unwrap();
    assert_eq!(api_tasks.len(), 1, "api key should see exactly 1 task");

    // Node-token caller lists tasks — should see none (it owns none).
    let node_list = mcp_call_tool(
        &client,
        &h.base_url,
        &node_token,
        "swarm_task_list",
        json!({}),
    )
    .await;
    let node_tasks = node_list["tasks"].as_array().unwrap();
    assert_eq!(
        node_tasks.len(),
        0,
        "node caller should see 0 tasks owned by api key"
    );
}
