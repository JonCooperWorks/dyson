//! `POST /mcp` — JSON-RPC 2.0 endpoint implementing the MCP server side.
//!
//! Tools exposed:
//!
//! - `list_nodes`             — enumerate registered nodes
//! - `swarm_status`           — counts (total, idle, busy, in-flight)
//! - `swarm_dispatch`         — sync: sign a task, push it, block on the result
//! - `swarm_submit`           — async: dispatch and return a task_id immediately
//! - `swarm_task_status`      — lightweight state + checkpoint counters
//! - `swarm_task_checkpoints` — progress events for a task, optionally filtered
//! - `swarm_task_result`      — final result (present once the task is terminal)
//! - `swarm_task_list`        — recent tasks across the whole hub
//!
//! Both dispatch paths (`swarm_dispatch` and `swarm_submit`) route
//! through the unified `TaskStore`.  The sync path registers a oneshot
//! waiter on the task record; the async path leaves it `None`.  The
//! result handler fires the waiter if present and writes the final
//! state into the same record in either case.
//!
//! The envelope matches `crates/dyson/src/skill/mcp/protocol.rs` — that is
//! how Dyson's MCP client talks to us.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::Json;
use axum::extract::State;
use dyson_swarm_protocol::types::{NodeStatus, Payload, SwarmResult, SwarmTask, TaskStatus};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::Hub;
use crate::queue::DispatchError;
use crate::registry::SseEvent;
use crate::router::{RoutingConstraints, select_node};
use crate::tasks::{TaskRecord, TaskSnapshot, TaskState};

/// Default timeout for a `swarm_dispatch` call when none is supplied.
const DEFAULT_DISPATCH_TIMEOUT: Duration = Duration::from_secs(600);

/// Default page size for `swarm_task_list`.
const DEFAULT_LIST_LIMIT: usize = 50;

/// Maximum characters stored as a `prompt_preview` on the TaskRecord.
const PROMPT_PREVIEW_CHARS: usize = 200;

/// Optional query parameters on the MCP endpoint.
///
/// `?caller=<node_name>` identifies the calling node so `list_nodes`
/// can exclude it from results (the node shouldn't see itself).
#[derive(serde::Deserialize, Default)]
pub struct McpQuery {
    caller: Option<String>,
}

/// The minimum JSON-RPC envelope we handle.
///
/// We deliberately parse into `Value` rather than a typed struct because
/// MCP clients sometimes send `id` as a string or omit it entirely for
/// notifications, and we want to be forgiving.
pub async fn mcp_handler(
    State(hub): State<Arc<Hub>>,
    axum::extract::Query(query): axum::extract::Query<McpQuery>,
    Json(request): Json<Value>,
) -> Json<Value> {
    let id = request.get("id").cloned();
    let method = request
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let params = request.get("params").cloned();

    tracing::debug!(method = %method, ?id, "MCP request");

    let result = match method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "dyson-swarm-hub",
                "version": env!("CARGO_PKG_VERSION"),
            }
        })),
        "notifications/initialized" => Ok(json!({})),
        "tools/list" => Ok(tools_list_response()),
        "tools/call" => handle_tools_call(&hub, query.caller.as_deref(), params).await,
        other => Err(McpError::method_not_found(other)),
    };

    Json(build_response(id, result))
}

/// Assemble the JSON-RPC response envelope.
fn build_response(id: Option<Value>, result: Result<Value, McpError>) -> Value {
    let mut envelope = serde_json::Map::new();
    envelope.insert("jsonrpc".into(), Value::from("2.0"));
    if let Some(id) = id {
        envelope.insert("id".into(), id);
    } else {
        envelope.insert("id".into(), Value::Null);
    }
    match result {
        Ok(v) => {
            envelope.insert("result".into(), v);
        }
        Err(e) => {
            envelope.insert(
                "error".into(),
                json!({ "code": e.code, "message": e.message }),
            );
        }
    }
    Value::Object(envelope)
}

/// A JSON-RPC error surfaced to the caller.
struct McpError {
    code: i64,
    message: String,
}

impl McpError {
    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("Method not found: {method}"),
        }
    }

    fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: msg.into(),
        }
    }
}

/// Definitions for `tools/list`.
fn tools_list_response() -> Value {
    json!({
        "tools": [
            {
                "name": "list_nodes",
                "description": "List every node registered with the swarm hub.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "swarm_status",
                "description": "Return counts of registered, idle, busy, and in-flight nodes/tasks.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "swarm_dispatch",
                "description": "Dispatch a task to an eligible node and block on the result. \
                    Best for short tasks (under the dispatch timeout). For long-running work \
                    like model fine-tuning, prefer swarm_submit.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string" },
                        "payloads": { "type": "array" },
                        "timeout_secs": { "type": "integer" },
                        "constraints": {
                            "type": "object",
                            "properties": {
                                "needs_gpu": { "type": "boolean" },
                                "needs_capability": { "type": "string" },
                                "min_ram_gb": { "type": "integer" }
                            },
                            "additionalProperties": false
                        }
                    },
                    "required": ["prompt"],
                    "additionalProperties": false
                }
            },
            {
                "name": "swarm_submit",
                "description": "Dispatch a long-running task to an eligible node and return \
                    a task_id immediately. Use this for model fine-tuning, large batch jobs, \
                    or anything that may run for minutes to hours. Poll swarm_task_status and \
                    swarm_task_checkpoints for progress, and swarm_task_result for the final \
                    SwarmResult once the task reaches a terminal state. Cancellation is not \
                    supported in v1.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string" },
                        "payloads": { "type": "array" },
                        "timeout_secs": {
                            "type": "integer",
                            "description": "Optional wall-clock timeout enforced by the node. \
                                Omit to let the task run as long as it needs."
                        },
                        "constraints": {
                            "type": "object",
                            "properties": {
                                "needs_gpu": { "type": "boolean" },
                                "needs_capability": { "type": "string" },
                                "min_ram_gb": { "type": "integer" }
                            },
                            "additionalProperties": false
                        }
                    },
                    "required": ["prompt"],
                    "additionalProperties": false
                }
            },
            {
                "name": "swarm_task_status",
                "description": "Return lightweight state for a previously submitted task: \
                    current state, checkpoint count, last sequence, timestamps.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string" }
                    },
                    "required": ["task_id"],
                    "additionalProperties": false
                }
            },
            {
                "name": "swarm_task_checkpoints",
                "description": "Return checkpoints emitted by a running (or completed) task, \
                    optionally filtered to sequence numbers strictly greater than \
                    since_sequence so callers can tail progress incrementally.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string" },
                        "since_sequence": { "type": "integer" }
                    },
                    "required": ["task_id"],
                    "additionalProperties": false
                }
            },
            {
                "name": "swarm_task_result",
                "description": "Return a task's final SwarmResult. While the task is still \
                    running, `result` is absent and `state` is `running`. Once the task is \
                    terminal the full result is present alongside the terminal state.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string" }
                    },
                    "required": ["task_id"],
                    "additionalProperties": false
                }
            },
            {
                "name": "swarm_task_list",
                "description": "List recent tasks on the hub, newest first, bounded by limit.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "limit": { "type": "integer" }
                    },
                    "additionalProperties": false
                }
            }
        ]
    })
}

/// Shared implementation for the `tools/call` dispatcher.
async fn handle_tools_call(
    hub: &Arc<Hub>,
    caller: Option<&str>,
    params: Option<Value>,
) -> Result<Value, McpError> {
    let params = params.ok_or_else(|| McpError::invalid_params("missing params"))?;
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| McpError::invalid_params("params.name is required"))?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    match name {
        "list_nodes" => Ok(tool_result_text(
            serde_json::to_string_pretty(&list_nodes(hub, caller).await).unwrap(),
            false,
        )),
        "swarm_status" => Ok(tool_result_text(
            serde_json::to_string_pretty(&swarm_status(hub).await).unwrap(),
            false,
        )),
        "swarm_dispatch" => match swarm_dispatch(hub, arguments).await {
            Ok(result) => Ok(tool_result_text(
                serde_json::to_string_pretty(&result).unwrap(),
                false,
            )),
            Err(e) => Ok(tool_result_text(format!("dispatch failed: {e}"), true)),
        },
        "swarm_submit" => match swarm_submit(hub, arguments).await {
            Ok(v) => Ok(tool_result_text(
                serde_json::to_string_pretty(&v).unwrap(),
                false,
            )),
            Err(e) => Ok(tool_result_text(format!("submit failed: {e}"), true)),
        },
        "swarm_task_status" => match swarm_task_status(hub, arguments).await {
            Ok(v) => Ok(tool_result_text(
                serde_json::to_string_pretty(&v).unwrap(),
                false,
            )),
            Err(e) => Ok(tool_result_text(e, true)),
        },
        "swarm_task_checkpoints" => match swarm_task_checkpoints(hub, arguments).await {
            Ok(v) => Ok(tool_result_text(
                serde_json::to_string_pretty(&v).unwrap(),
                false,
            )),
            Err(e) => Ok(tool_result_text(e, true)),
        },
        "swarm_task_result" => match swarm_task_result(hub, arguments).await {
            Ok(v) => Ok(tool_result_text(
                serde_json::to_string_pretty(&v).unwrap(),
                false,
            )),
            Err(e) => Ok(tool_result_text(e, true)),
        },
        "swarm_task_list" => Ok(tool_result_text(
            serde_json::to_string_pretty(&swarm_task_list(hub, arguments).await).unwrap(),
            false,
        )),
        other => Err(McpError::invalid_params(format!("unknown tool: {other}"))),
    }
}

/// Build an MCP `tools/call` result.  A single text content block.
fn tool_result_text(text: String, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error
    })
}

/// `list_nodes` — registered nodes, excluding `caller` if set.
async fn list_nodes(hub: &Arc<Hub>, caller: Option<&str>) -> Value {
    hub.registry
        .with_entries(|entries| {
            let mut rows: Vec<Value> = entries
                .values()
                .filter(|entry| caller.is_none_or(|c| entry.manifest.node_name != c))
                .map(|entry| {
                    json!({
                        "node_id": entry.node_id,
                        "node_name": entry.manifest.node_name,
                        "status": status_label(&entry.status),
                        "capabilities": entry.manifest.capabilities,
                        "hardware": {
                            "ram_bytes": entry.manifest.hardware.ram_bytes,
                            "cpus": entry.manifest.hardware.cpus.len(),
                            "gpus": entry.manifest.hardware.gpus.len(),
                        }
                    })
                })
                .collect();
            rows.sort_by(|a, b| {
                a["node_id"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["node_id"].as_str().unwrap_or(""))
            });
            Value::Array(rows)
        })
        .await
}

fn status_label(status: &NodeStatus) -> &'static str {
    match status {
        NodeStatus::Idle => "idle",
        NodeStatus::Busy { .. } => "busy",
        NodeStatus::Draining => "draining",
    }
}

/// `swarm_status` — aggregate counts.
async fn swarm_status(hub: &Arc<Hub>) -> Value {
    let counts = hub.registry.counts().await;
    let in_flight = hub.tasks.len().await;
    json!({
        "nodes_total": counts.total,
        "nodes_idle": counts.idle,
        "nodes_busy": counts.busy,
        "tasks_pending": 0,
        "tasks_tracked": in_flight,
    })
}

// ---------------------------------------------------------------------------
// Dispatch plumbing shared by swarm_dispatch and swarm_submit
// ---------------------------------------------------------------------------

/// Fields parsed out of a dispatch/submit tool-call's arguments.
struct DispatchArgs {
    prompt: String,
    payloads: Vec<Payload>,
    timeout_secs: Option<u64>,
    constraints: RoutingConstraints,
}

fn parse_dispatch_args(arguments: Value) -> Result<DispatchArgs, DispatchError> {
    let prompt = arguments
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| DispatchError::Cancelled("missing prompt".into()))?
        .to_string();

    let payloads: Vec<Payload> = match arguments.get("payloads") {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone())
            .map_err(|e| DispatchError::Cancelled(format!("invalid payloads: {e}")))?,
        _ => vec![],
    };

    let timeout_secs = arguments.get("timeout_secs").and_then(|v| v.as_u64());

    let constraints = parse_constraints(&arguments)?;

    Ok(DispatchArgs {
        prompt,
        payloads,
        timeout_secs,
        constraints,
    })
}

/// Outcome of placing a task on a node.  Shared by sync and async paths.
struct PlacedTask {
    task_id: String,
    node_id: String,
    submitted_at: SystemTime,
}

/// Select a node, sign the task, insert a TaskRecord (with optional
/// waiter), flip the node to Busy, and push the signed task down the
/// node's SSE stream.
///
/// On any failure after the TaskRecord is inserted, the record is
/// finalized as Failed so polling callers see a consistent story.
async fn place_task(
    hub: &Arc<Hub>,
    args: &DispatchArgs,
    waiter: Option<oneshot::Sender<SwarmResult>>,
) -> Result<PlacedTask, DispatchError> {
    let node_id = select_node(&hub.registry, &args.constraints)
        .await
        .ok_or(DispatchError::NoEligibleNode)?;

    let task = SwarmTask {
        task_id: uuid::Uuid::new_v4().to_string(),
        prompt: args.prompt.clone(),
        payloads: args.payloads.clone(),
        timeout_secs: args.timeout_secs,
    };

    let canonical = serde_json::to_vec(&task)
        .map_err(|e| DispatchError::Cancelled(format!("task serialization failed: {e}")))?;
    let wire = hub.key.sign_task(&canonical);

    let submitted_at = SystemTime::now();
    let record = TaskRecord {
        task_id: task.task_id.clone(),
        node_id: node_id.clone(),
        prompt_preview: truncate_prompt(&args.prompt),
        submitted_at,
        last_update: submitted_at,
        state: TaskState::Running,
        checkpoints: Vec::new(),
        result: None,
        waiter,
    };
    hub.tasks.insert(record).await;

    hub.registry
        .set_status(
            &node_id,
            NodeStatus::Busy {
                task_id: task.task_id.clone(),
            },
        )
        .await;

    let pushed = hub
        .registry
        .push_event(&node_id, SseEvent::Task(wire))
        .await;
    if !pushed {
        // Record the failure in the TaskStore (so polling callers see
        // Failed rather than a dangling Running), flip the node back to
        // Idle, and surface the error to the caller.
        let failed = SwarmResult {
            task_id: task.task_id.clone(),
            text: String::new(),
            payloads: vec![],
            status: TaskStatus::Failed {
                error: format!("node '{node_id}' has no active SSE stream"),
            },
            duration_secs: 0,
        };
        // Any sync waiter we stashed is dropped here — the caller will
        // observe the DispatchError::Cancelled return value instead.
        let _ = hub.tasks.finalize(&task.task_id, failed).await;
        hub.registry.set_status(&node_id, NodeStatus::Idle).await;
        return Err(DispatchError::Cancelled(format!(
            "node '{node_id}' has no active SSE stream"
        )));
    }

    tracing::info!(
        node_id = %node_id,
        task_id = %task.task_id,
        "placed task on node"
    );

    Ok(PlacedTask {
        task_id: task.task_id,
        node_id,
        submitted_at,
    })
}

fn truncate_prompt(prompt: &str) -> String {
    // Walk char boundaries so we never slice a UTF-8 code point in half.
    if prompt.len() <= PROMPT_PREVIEW_CHARS {
        return prompt.to_string();
    }
    let mut end = PROMPT_PREVIEW_CHARS;
    while end > 0 && !prompt.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &prompt[..end])
}

// ---------------------------------------------------------------------------
// swarm_dispatch — sync path
// ---------------------------------------------------------------------------

/// `swarm_dispatch` — sign a task, push it, wait for the result.
async fn swarm_dispatch(hub: &Arc<Hub>, arguments: Value) -> Result<SwarmResult, DispatchError> {
    let args = parse_dispatch_args(arguments)?;
    let timeout = args
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_DISPATCH_TIMEOUT);

    let (tx, rx) = oneshot::channel::<SwarmResult>();
    let placed = place_task(hub, &args, Some(tx)).await?;

    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => {
            if let TaskStatus::Failed { error } = &result.status {
                tracing::warn!(task_id = %result.task_id, %error, "task failed on node");
            }
            Ok(result)
        }
        Ok(Err(_)) => {
            // Result channel closed before a value arrived.  The record
            // already reflects whatever state the result handler wrote.
            hub.tasks.abandon_waiter(&placed.task_id).await;
            Err(DispatchError::Cancelled("result channel closed".into()))
        }
        Err(_) => {
            // Sync dispatcher timed out waiting.  Clear the waiter so a
            // late result still stores into the record but doesn't try
            // to fire a dead channel.
            hub.tasks.abandon_waiter(&placed.task_id).await;
            Err(DispatchError::Timeout)
        }
    }
}

// ---------------------------------------------------------------------------
// swarm_submit — async path
// ---------------------------------------------------------------------------

/// `swarm_submit` — dispatch and return a task_id immediately.
async fn swarm_submit(hub: &Arc<Hub>, arguments: Value) -> Result<Value, DispatchError> {
    let args = parse_dispatch_args(arguments)?;
    let placed = place_task(hub, &args, None).await?;

    Ok(json!({
        "task_id": placed.task_id,
        "node_id": placed.node_id,
        "submitted_at_unix": placed
            .submitted_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "state": "running",
    }))
}

// ---------------------------------------------------------------------------
// Read-side MCP tools
// ---------------------------------------------------------------------------

fn required_task_id(arguments: &Value) -> Result<String, String> {
    arguments
        .get("task_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "task_id is required".to_string())
}

async fn swarm_task_status(hub: &Arc<Hub>, arguments: Value) -> Result<Value, String> {
    let task_id = required_task_id(&arguments)?;
    let snap = hub
        .tasks
        .get(&task_id)
        .await
        .ok_or_else(|| format!("unknown task_id: {task_id}"))?;
    Ok(status_json(&snap))
}

fn status_json(snap: &TaskSnapshot) -> Value {
    let last_sequence = snap.checkpoints.last().map(|c| c.sequence).unwrap_or(0);
    let state_value = serde_json::to_value(&snap.state).unwrap_or(Value::Null);
    json!({
        "task_id": snap.task_id,
        "node_id": snap.node_id,
        "prompt_preview": snap.prompt_preview,
        "state": state_value,
        "checkpoint_count": snap.checkpoints.len(),
        "last_sequence": last_sequence,
        "submitted_at_unix": snap.submitted_at_unix,
        "last_update_unix": snap.last_update_unix,
    })
}

async fn swarm_task_checkpoints(hub: &Arc<Hub>, arguments: Value) -> Result<Value, String> {
    let task_id = required_task_id(&arguments)?;
    let since = arguments
        .get("since_sequence")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(0);
    let cps = hub
        .tasks
        .checkpoints_since(&task_id, since)
        .await
        .ok_or_else(|| format!("unknown task_id: {task_id}"))?;
    Ok(json!({
        "task_id": task_id,
        "since_sequence": since,
        "checkpoints": cps,
    }))
}

async fn swarm_task_result(hub: &Arc<Hub>, arguments: Value) -> Result<Value, String> {
    let task_id = required_task_id(&arguments)?;
    let snap = hub
        .tasks
        .get(&task_id)
        .await
        .ok_or_else(|| format!("unknown task_id: {task_id}"))?;
    let state_value = serde_json::to_value(&snap.state).unwrap_or(Value::Null);
    Ok(match snap.result {
        Some(r) => json!({
            "task_id": snap.task_id,
            "state": state_value,
            "result": r,
        }),
        None => json!({
            "task_id": snap.task_id,
            "state": state_value,
        }),
    })
}

async fn swarm_task_list(hub: &Arc<Hub>, arguments: Value) -> Value {
    let limit = arguments
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_LIST_LIMIT);
    let snaps = hub.tasks.list(limit).await;
    let rows: Vec<Value> = snaps.iter().map(status_json).collect();
    json!({
        "tasks": rows,
        "count": rows.len(),
    })
}

fn parse_constraints(arguments: &Value) -> Result<RoutingConstraints, DispatchError> {
    let Some(obj) = arguments.get("constraints") else {
        return Ok(RoutingConstraints::default());
    };

    if obj.is_null() {
        return Ok(RoutingConstraints::default());
    }

    let needs_gpu = obj
        .get("needs_gpu")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let needs_capability = obj
        .get("needs_capability")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let min_ram_gb = obj.get("min_ram_gb").and_then(|v| v.as_u64());

    Ok(RoutingConstraints {
        needs_gpu,
        needs_capability,
        min_ram_gb,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dyson_swarm_protocol::types::TaskCheckpoint;

    #[test]
    fn parse_constraints_all_fields() {
        let args = json!({
            "prompt": "hi",
            "constraints": {
                "needs_gpu": true,
                "needs_capability": "bash",
                "min_ram_gb": 8
            }
        });
        let c = parse_constraints(&args).unwrap();
        assert!(c.needs_gpu);
        assert_eq!(c.needs_capability.as_deref(), Some("bash"));
        assert_eq!(c.min_ram_gb, Some(8));
    }

    #[test]
    fn parse_constraints_missing_is_default() {
        let args = json!({ "prompt": "hi" });
        let c = parse_constraints(&args).unwrap();
        assert!(!c.needs_gpu);
        assert!(c.needs_capability.is_none());
        assert_eq!(c.min_ram_gb, None);
    }

    #[test]
    fn truncate_prompt_respects_char_boundaries() {
        let short = "hello";
        assert_eq!(truncate_prompt(short), "hello");

        let long = "a".repeat(PROMPT_PREVIEW_CHARS + 20);
        let truncated = truncate_prompt(&long);
        assert!(truncated.ends_with("..."));
        assert!(truncated.len() <= PROMPT_PREVIEW_CHARS + 3);

        // UTF-8 multi-byte safety: a string full of 3-byte chars near
        // the boundary must still produce valid UTF-8.
        let utf = "é".repeat(200);
        let t = truncate_prompt(&utf);
        assert!(t.is_char_boundary(t.len() - 3)); // right before the "..."
    }

    #[tokio::test]
    async fn status_json_reports_checkpoint_count_and_last_sequence() {
        let store = crate::tasks::TaskStore::new();
        store
            .insert(crate::tasks::TaskRecord {
                task_id: "t1".into(),
                node_id: "node-a".into(),
                prompt_preview: "do a thing".into(),
                submitted_at: SystemTime::now(),
                last_update: SystemTime::now(),
                state: TaskState::Running,
                checkpoints: vec![],
                result: None,
                waiter: None,
            })
            .await;
        for seq in 1..=3 {
            store
                .append_checkpoint(TaskCheckpoint {
                    task_id: "t1".into(),
                    sequence: seq,
                    message: format!("s{seq}"),
                    progress: None,
                    emitted_at_secs: 0,
                })
                .await;
        }
        let snap = store.get("t1").await.unwrap();
        let v = status_json(&snap);
        assert_eq!(v["checkpoint_count"], json!(3));
        assert_eq!(v["last_sequence"], json!(3));
        assert_eq!(v["state"]["state"], json!("running"));
    }

    #[test]
    fn parse_dispatch_args_accepts_null_payloads() {
        let args = json!({
            "prompt": "hi",
            "payloads": null,
        });
        let parsed = parse_dispatch_args(args).unwrap();
        assert!(parsed.payloads.is_empty());
    }
}
