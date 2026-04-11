//! `POST /mcp` — JSON-RPC 2.0 endpoint implementing the MCP server side.
//!
//! Tools exposed:
//!
//! - `list_nodes`    — enumerate registered nodes
//! - `swarm_status`  — counts (total, idle, busy, in-flight) — with an
//!                     optional `task_id` filter for one-task lookup
//! - `swarm_dispatch`— legacy synchronous dispatch (deprecated; use
//!                     `swarm_submit` + `swarm_await` instead)
//! - `swarm_submit`  — submit a long-running task and return the task_id
//!                     immediately (non-blocking, fire-and-forget)
//! - `swarm_await`   — block on a task_id with a configurable deadline
//! - `swarm_logs`    — tail captured logs / progress messages
//! - `swarm_cancel`  — request cancellation; worker checkpoints and exits
//! - `swarm_results` — list recent terminal tasks ("what happened
//!                     overnight?" reachback for agents)
//!
//! The envelope matches `crates/dyson/src/skill/mcp/protocol.rs` — that is
//! how Dyson's MCP client talks to us.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use dyson_swarm_protocol::types::{NodeStatus, Payload, SwarmResult, SwarmTask, TaskStatus};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::Hub;
use crate::queue::DispatchError;
use crate::registry::SseEvent;
use crate::router::{RoutingConstraints, select_node};
use crate::scheduler::{NotifyChannel, SubmitRequest, TaskState, TerminalStatus};

/// Default timeout for a `swarm_dispatch` call when none is supplied.
const DEFAULT_DISPATCH_TIMEOUT: Duration = Duration::from_secs(600);

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
    let constraints_schema = json!({
        "type": "object",
        "properties": {
            "needs_gpu": { "type": "boolean" },
            "needs_capability": { "type": "string" },
            "min_ram_gb": { "type": "integer" }
        },
        "additionalProperties": false
    });

    let notify_schema = json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": ["stdout", "webhook", "telegram"] },
                "url": { "type": "string" },
                "bot_token": { "type": "string" },
                "chat_id": { "type": "string" },
                "template": { "type": "string" }
            },
            "additionalProperties": true
        }
    });

    json!({
        "tools": [
            {
                "name": "list_nodes",
                "description": "List every node registered with the swarm hub.",
                "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
            },
            {
                "name": "swarm_status",
                "description": "Return swarm-wide counts. Pass task_id to get the state of one task instead.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "task_id": { "type": "string" } },
                    "additionalProperties": false
                }
            },
            {
                "name": "swarm_dispatch",
                "description": "DEPRECATED: synchronous dispatch. Use swarm_submit + swarm_await for new code.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string" },
                        "payloads": { "type": "array" },
                        "timeout_secs": { "type": "integer" },
                        "constraints": constraints_schema
                    },
                    "required": ["prompt"],
                    "additionalProperties": false
                }
            },
            {
                "name": "swarm_submit",
                "description": "Submit a (potentially long-running) task and return its task_id immediately. Use swarm_status / swarm_results to track it; use the notify field to get a Telegram or webhook ping when it finishes.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string" },
                        "skill": { "type": "string", "description": "Hint: route to a node that advertises this capability." },
                        "payloads": { "type": "array" },
                        "timeout_secs": { "type": "integer" },
                        "constraints": constraints_schema,
                        "notify": notify_schema
                    },
                    "required": ["prompt"],
                    "additionalProperties": false
                }
            },
            {
                "name": "swarm_await",
                "description": "Block on a task_id for up to wait_secs seconds. Returns the same shape as swarm_dispatch when the task finishes; returns the current state if the deadline expires.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string" },
                        "wait_secs": { "type": "integer", "default": 60 }
                    },
                    "required": ["task_id"],
                    "additionalProperties": false
                }
            },
            {
                "name": "swarm_logs",
                "description": "Tail logs and progress messages for a task.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string" },
                        "since_seq": { "type": "integer", "default": -1 },
                        "limit": { "type": "integer", "default": 100 }
                    },
                    "required": ["task_id"],
                    "additionalProperties": false
                }
            },
            {
                "name": "swarm_cancel",
                "description": "Request cancellation. The worker is asked to checkpoint and exit; final state lands as 'cancelled' (with optional checkpoint blob in the result).",
                "inputSchema": {
                    "type": "object",
                    "properties": { "task_id": { "type": "string" } },
                    "required": ["task_id"],
                    "additionalProperties": false
                }
            },
            {
                "name": "swarm_results",
                "description": "List recent terminal tasks (done / failed / cancelled). Use this on agent startup to catch up on what happened while you were gone.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "since_ms": { "type": "integer" },
                        "limit": { "type": "integer", "default": 25 },
                        "states": {
                            "type": "array",
                            "items": { "type": "string", "enum": ["done", "failed", "cancelled"] }
                        }
                    },
                    "additionalProperties": false
                }
            }
        ]
    })
}

/// Shared implementation for the `tools/call` dispatcher.
async fn handle_tools_call(hub: &Arc<Hub>, caller: Option<&str>, params: Option<Value>) -> Result<Value, McpError> {
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
        "swarm_status" => match swarm_status_tool(hub, &arguments).await {
            Ok(v) => Ok(tool_result_text(serde_json::to_string_pretty(&v).unwrap(), false)),
            Err(e) => Ok(tool_result_text(format!("swarm_status failed: {e}"), true)),
        },
        "swarm_dispatch" => match swarm_dispatch(hub, arguments).await {
            Ok(result) => Ok(tool_result_text(
                serde_json::to_string_pretty(&result).unwrap(),
                false,
            )),
            Err(e) => Ok(tool_result_text(format!("dispatch failed: {e}"), true)),
        },
        "swarm_submit" => match swarm_submit_tool(hub, arguments).await {
            Ok(v) => Ok(tool_result_text(serde_json::to_string_pretty(&v).unwrap(), false)),
            Err(e) => Ok(tool_result_text(format!("swarm_submit failed: {e}"), true)),
        },
        "swarm_await" => match swarm_await_tool(hub, arguments).await {
            Ok(v) => Ok(tool_result_text(serde_json::to_string_pretty(&v).unwrap(), false)),
            Err(e) => Ok(tool_result_text(format!("swarm_await failed: {e}"), true)),
        },
        "swarm_logs" => match swarm_logs_tool(hub, arguments).await {
            Ok(v) => Ok(tool_result_text(serde_json::to_string_pretty(&v).unwrap(), false)),
            Err(e) => Ok(tool_result_text(format!("swarm_logs failed: {e}"), true)),
        },
        "swarm_cancel" => match swarm_cancel_tool(hub, arguments).await {
            Ok(v) => Ok(tool_result_text(serde_json::to_string_pretty(&v).unwrap(), false)),
            Err(e) => Ok(tool_result_text(format!("swarm_cancel failed: {e}"), true)),
        },
        "swarm_results" => match swarm_results_tool(hub, arguments).await {
            Ok(v) => Ok(tool_result_text(serde_json::to_string_pretty(&v).unwrap(), false)),
            Err(e) => Ok(tool_result_text(format!("swarm_results failed: {e}"), true)),
        },
        other => Err(McpError::invalid_params(format!(
            "unknown tool: {other}"
        ))),
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
                .filter(|entry| caller.map_or(true, |c| entry.manifest.node_name != c))
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
                a["node_id"].as_str().unwrap_or("").cmp(b["node_id"].as_str().unwrap_or(""))
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

/// `swarm_status` — aggregate counts (no args), or one task's state if
/// `task_id` is supplied.
async fn swarm_status_tool(hub: &Arc<Hub>, arguments: &Value) -> Result<Value, String> {
    if let Some(task_id) = arguments.get("task_id").and_then(|v| v.as_str()) {
        return match hub.tasks.get(task_id).await {
            Ok(Some(row)) => Ok(task_row_json(&row)),
            Ok(None) => Err(format!("task not found: {task_id}")),
            Err(e) => Err(format!("task store: {e}")),
        };
    }

    let counts = hub.registry.counts().await;
    let in_flight = hub.pending_dispatches.lock().await.len();
    Ok(json!({
        "nodes_total": counts.total,
        "nodes_idle": counts.idle,
        "nodes_busy": counts.busy,
        "tasks_in_flight": in_flight,
    }))
}

/// Compact JSON view of one task row.
fn task_row_json(row: &crate::scheduler::TaskRow) -> Value {
    json!({
        "task_id": row.task_id,
        "state": row.state.as_str(),
        "skill": row.skill,
        "prompt": row.prompt,
        "assigned_node": row.assigned_node,
        "submitted_at_ms": st_to_ms(row.submitted_at),
        "started_at_ms": row.started_at.map(st_to_ms),
        "finished_at_ms": row.finished_at.map(st_to_ms),
        "last_progress_at_ms": row.last_progress_at.map(st_to_ms),
        "progress_pct": row.progress_pct,
        "progress_message": row.progress_message,
        "result_text": row.result_text,
        "error": row.error,
        "duration_secs": row.duration_secs,
        "notification_delivered": row.notification_delivered,
    })
}

fn st_to_ms(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// New tools: swarm_submit / swarm_await / swarm_logs / swarm_cancel /
// swarm_results — the long-running task surface.
// ---------------------------------------------------------------------------

/// `swarm_submit` — non-blocking submission. Inserts a row in the
/// scheduler's task store and (if a worker is idle) immediately
/// dispatches; returns the task_id either way. Notification channels
/// from the request body are persisted on the row and fired when the
/// task reaches a terminal state.
async fn swarm_submit_tool(hub: &Arc<Hub>, arguments: Value) -> Result<Value, String> {
    let req: SubmitRequest = serde_json::from_value(arguments)
        .map_err(|e| format!("invalid swarm_submit body: {e}"))?;

    let constraints = req
        .constraints
        .clone()
        .unwrap_or_default()
        .into_routing(req.skill.clone());
    let constraints_json = serde_json::json!({
        "needs_gpu": constraints.needs_gpu,
        "needs_capability": constraints.needs_capability,
        "min_ram_gb": constraints.min_ram_gb,
    });

    let task_id = hub
        .tasks
        .submit(
            req.skill.clone(),
            req.prompt.clone(),
            req.payloads.clone(),
            req.timeout_secs,
            constraints_json,
            req.notify.clone(),
        )
        .await
        .map_err(|e| format!("task store: {e}"))?;

    // Try to dispatch right now if a worker is idle. If nothing matches,
    // the task stays Pending; the caller can poll, or the next
    // sweep / heartbeat path can pick it up.
    let dispatched = try_dispatch_pending(hub, &task_id, &req, &constraints).await;
    Ok(json!({
        "task_id": task_id,
        "state": if dispatched { "assigned" } else { "pending" },
        "dispatched_immediately": dispatched,
    }))
}

/// Build + sign a SwarmTask for an existing task_id and push it over SSE
/// to a chosen worker. Updates the scheduler row to `assigned` on success.
async fn try_dispatch_pending(
    hub: &Arc<Hub>,
    task_id: &str,
    req: &SubmitRequest,
    constraints: &RoutingConstraints,
) -> bool {
    let Some(node_id) = select_node(&hub.registry, constraints).await else {
        return false;
    };

    let swarm_task = SwarmTask {
        task_id: task_id.to_string(),
        prompt: req.prompt.clone(),
        payloads: req.payloads.clone(),
        timeout_secs: req.timeout_secs,
    };
    let canonical = match serde_json::to_vec(&swarm_task) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "swarm_submit: serialize failed");
            return false;
        }
    };
    let wire = hub.key.sign_task(&canonical);

    hub.registry
        .set_status(
            &node_id,
            NodeStatus::Busy {
                task_id: task_id.to_string(),
            },
        )
        .await;
    let pushed = hub
        .registry
        .push_event(&node_id, SseEvent::Task(wire))
        .await;
    if !pushed {
        hub.registry.set_status(&node_id, NodeStatus::Idle).await;
        return false;
    }
    if let Err(e) = hub.tasks.mark_assigned(task_id, &node_id).await {
        tracing::warn!(error = %e, "swarm_submit: mark_assigned failed");
    }
    true
}

/// `swarm_await` — block on a task_id for up to `wait_secs` seconds.
async fn swarm_await_tool(hub: &Arc<Hub>, arguments: Value) -> Result<Value, String> {
    let task_id = arguments
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("task_id required")?
        .to_string();
    let wait_secs = arguments
        .get("wait_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(60);

    let deadline = std::time::Instant::now() + Duration::from_secs(wait_secs);
    loop {
        let row = hub
            .tasks
            .get(&task_id)
            .await
            .map_err(|e| format!("task store: {e}"))?
            .ok_or_else(|| format!("task not found: {task_id}"))?;

        if row.is_terminal() || std::time::Instant::now() >= deadline {
            return Ok(task_row_json(&row));
        }
        // Short polling interval — submit/poll model means we don't
        // need to worry about subscription churn.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// `swarm_logs` — paginated log tail.
async fn swarm_logs_tool(hub: &Arc<Hub>, arguments: Value) -> Result<Value, String> {
    let task_id = arguments
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("task_id required")?;
    let since_seq = arguments.get("since_seq").and_then(|v| v.as_i64()).unwrap_or(-1);
    let limit = arguments.get("limit").and_then(|v| v.as_i64()).unwrap_or(100);
    let rows = hub
        .tasks
        .read_logs(task_id, since_seq, limit)
        .await
        .map_err(|e| format!("task store: {e}"))?;
    let entries: Vec<Value> = rows
        .into_iter()
        .map(|(seq, ts_ms, chunk)| json!({"seq": seq, "ts_ms": ts_ms, "chunk": chunk}))
        .collect();
    Ok(json!({"task_id": task_id, "entries": entries}))
}

/// `swarm_cancel` — request cancellation, push a cancel SSE event to
/// the assigned worker.
async fn swarm_cancel_tool(hub: &Arc<Hub>, arguments: Value) -> Result<Value, String> {
    let task_id = arguments
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("task_id required")?
        .to_string();

    let row = hub
        .tasks
        .get(&task_id)
        .await
        .map_err(|e| format!("task store: {e}"))?
        .ok_or_else(|| format!("task not found: {task_id}"))?;
    if row.is_terminal() {
        return Ok(json!({
            "task_id": task_id,
            "state": row.state.as_str(),
            "noop": true,
        }));
    }

    let transitioned = hub
        .tasks
        .request_cancel(&task_id)
        .await
        .map_err(|e| format!("task store: {e}"))?;
    if !transitioned {
        // Already cancelling.
        return Ok(json!({"task_id": task_id, "state": "cancelling", "noop": true}));
    }

    if let Some(node_id) = row.assigned_node.as_ref() {
        let pushed = hub
            .registry
            .push_event(node_id, SseEvent::Cancel { task_id: task_id.clone() })
            .await;
        if !pushed {
            tracing::warn!(%task_id, %node_id, "cancel SSE push failed (worker offline)");
        }
    }

    Ok(json!({"task_id": task_id, "state": "cancelling"}))
}

/// `swarm_results` — list recent terminal tasks.
async fn swarm_results_tool(hub: &Arc<Hub>, arguments: Value) -> Result<Value, String> {
    let since_ms = arguments.get("since_ms").and_then(|v| v.as_u64());
    let limit = arguments.get("limit").and_then(|v| v.as_i64()).unwrap_or(25);
    let states_owned: Option<Vec<String>> = arguments
        .get("states")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .collect()
        });
    let states_refs: Option<Vec<&str>> = states_owned
        .as_ref()
        .map(|v| v.iter().map(|s| s.as_str()).collect());

    let rows = hub
        .tasks
        .recent_results(since_ms, limit, states_refs.as_deref())
        .await
        .map_err(|e| format!("task store: {e}"))?;
    Ok(json!({"results": rows}))
}

// Keep linter happy: NotifyChannel / TaskState / TerminalStatus are
// pulled in for documentation linkage in docs/swarm.md and to keep the
// re-export surface explicit.
#[allow(dead_code)]
fn _imports_kept() {
    let _ = std::mem::size_of::<NotifyChannel>();
    let _ = std::mem::size_of::<TaskState>();
    let _ = std::mem::size_of::<TerminalStatus>();
}

/// `swarm_dispatch` — sign a task, push it, wait for the result.
async fn swarm_dispatch(hub: &Arc<Hub>, arguments: Value) -> Result<SwarmResult, DispatchError> {
    let prompt = arguments
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| DispatchError::Cancelled("missing prompt".into()))?
        .to_string();

    let payloads: Vec<Payload> = match arguments.get("payloads") {
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| DispatchError::Cancelled(format!("invalid payloads: {e}")))?,
        None => vec![],
    };

    let timeout = arguments
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_DISPATCH_TIMEOUT);

    let constraints = parse_constraints(&arguments)?;

    let node_id = select_node(&hub.registry, &constraints)
        .await
        .ok_or(DispatchError::NoEligibleNode)?;

    // Build and sign the task.
    let task = SwarmTask {
        task_id: uuid::Uuid::new_v4().to_string(),
        prompt,
        payloads,
        timeout_secs: arguments
            .get("timeout_secs")
            .and_then(|v| v.as_u64()),
    };
    let canonical = serde_json::to_vec(&task)
        .map_err(|e| DispatchError::Cancelled(format!("task serialization failed: {e}")))?;
    let wire = hub.key.sign_task(&canonical);

    // Register the oneshot BEFORE pushing the task so results can't race us.
    let (tx, rx) = oneshot::channel::<SwarmResult>();
    {
        let mut pending = hub.pending_dispatches.lock().await;
        pending.insert(task.task_id.clone(), tx);
    }

    // Flip the node to Busy before pushing.
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
        // Clean up: drop the pending entry and flip the node back to idle.
        hub.pending_dispatches
            .lock()
            .await
            .remove(&task.task_id);
        hub.registry.set_status(&node_id, NodeStatus::Idle).await;
        return Err(DispatchError::Cancelled(format!(
            "node '{node_id}' has no active SSE stream"
        )));
    }

    tracing::info!(
        node_id = %node_id,
        task_id = %task.task_id,
        "dispatched task over SSE"
    );

    // Block on the result.
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => {
            // If the task reported failure, surface it.  If it succeeded
            // or was cancelled, hand the full result to the caller.
            if let TaskStatus::Failed { error } = &result.status {
                tracing::warn!(task_id = %result.task_id, %error, "task failed on node");
            }
            Ok(result)
        }
        Ok(Err(_)) => {
            hub.pending_dispatches
                .lock()
                .await
                .remove(&task.task_id);
            Err(DispatchError::Cancelled("result channel closed".into()))
        }
        Err(_) => {
            hub.pending_dispatches
                .lock()
                .await
                .remove(&task.task_id);
            Err(DispatchError::Timeout)
        }
    }
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
}
