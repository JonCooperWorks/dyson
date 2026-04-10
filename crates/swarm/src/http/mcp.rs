//! `POST /mcp` — JSON-RPC 2.0 endpoint implementing the MCP server side.
//!
//! Tools exposed:
//!
//! - `list_nodes`    — enumerate registered nodes
//! - `swarm_status`  — counts (total, idle, busy, in-flight)
//! - `swarm_dispatch`— sign a task, push it over SSE, block on the result
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

/// Default timeout for a `swarm_dispatch` call when none is supplied.
const DEFAULT_DISPATCH_TIMEOUT: Duration = Duration::from_secs(600);

/// The minimum JSON-RPC envelope we handle.
///
/// We deliberately parse into `Value` rather than a typed struct because
/// MCP clients sometimes send `id` as a string or omit it entirely for
/// notifications, and we want to be forgiving.
/// Optional query parameters on the MCP endpoint.
///
/// `?caller=<node_name>` identifies the calling node so `list_nodes`
/// can exclude it from results (the node shouldn't see itself).
#[derive(serde::Deserialize, Default)]
pub struct McpQuery {
    caller: Option<String>,
}

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
                "description": "Dispatch a task to an eligible node and return the result.",
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

/// `list_nodes` — a JSON array of the registered nodes.
///
/// When `caller` is set (from `?caller=<node_name>` on the MCP endpoint),
/// the calling node is excluded from results so it only sees its peers.
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

/// `swarm_status` — aggregate counts.
async fn swarm_status(hub: &Arc<Hub>) -> Value {
    let counts = hub.registry.counts().await;
    let in_flight = hub.pending_dispatches.lock().await.len();
    json!({
        "nodes_total": counts.total,
        "nodes_idle": counts.idle,
        "nodes_busy": counts.busy,
        "tasks_pending": 0,
        "tasks_in_flight": in_flight,
    })
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
