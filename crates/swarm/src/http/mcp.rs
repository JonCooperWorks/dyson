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
use axum::http::HeaderMap;
use dyson_swarm_protocol::types::{NodeStatus, Payload, SwarmResult, SwarmTask, TaskStatus};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::Hub;
use crate::queue::DispatchError;
use crate::registry::{NodeId, SseEvent};
use crate::router::{RoutingConstraints, select_node, select_node_by_id};
use crate::tasks::{TaskRecord, TaskSnapshot, TaskState};

/// Verified identity of the MCP caller.
///
/// Resolved from a bearer token — either a node registration token
/// or a static API key.  All `tools/call` requests require a valid
/// caller; unauthenticated requests are rejected.
struct McpCaller {
    /// Node ID (registration token) or synthetic owner ID (API key).
    node_id: String,
    /// Node name for `list_nodes` self-exclusion (absent for API keys).
    node_name: Option<String>,
}

/// Verify the bearer token as a static API key against the configured
/// argon2id hash.  Returns `Some(McpCaller)` with the pre-computed
/// owner ID on success.
fn try_api_key_auth(hub: &Hub, token: &str) -> Option<McpCaller> {
    let key = hub.mcp_api_key.as_ref()?;
    key.verify(token).then(|| McpCaller {
        node_id: key.owner_id.clone(),
        node_name: None,
    })
}

/// Default timeout for a `swarm_dispatch` call when none is supplied.
const DEFAULT_DISPATCH_TIMEOUT: Duration = Duration::from_secs(600);

/// Default page size for `swarm_task_list`.
const DEFAULT_LIST_LIMIT: usize = 50;

/// Maximum characters stored as a `prompt_preview` on the TaskRecord.
const PROMPT_PREVIEW_CHARS: usize = 200;

/// The minimum JSON-RPC envelope we handle.
///
/// We deliberately parse into `Value` rather than a typed struct because
/// MCP clients sometimes send `id` as a string or omit it entirely for
/// notifications, and we want to be forgiving.
pub async fn mcp_handler(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
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

    // Resolve caller identity from bearer token.  Only `initialize`
    // and `notifications/initialized` are open; both `tools/list` and
    // `tools/call` require a valid bearer.
    let caller = match crate::auth::extract_bearer(&headers) {
        Some(token) => {
            // Fast path: node registration token (O(1) HashMap lookup).
            match hub.registry.node_id_for_token(&token).await {
                Some(node_id) => {
                    let node_name = hub
                        .registry
                        .with_entry(&node_id, |e| e.manifest.node_name.clone())
                        .await;
                    Some(McpCaller { node_id, node_name })
                }
                // Slow path: try static API key (argon2id, ~30-50ms).
                None => try_api_key_auth(&hub, &token),
            }
        }
        None => None,
    };

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
        // `tools/list` and `tools/call` both require auth.  An
        // unauthenticated tools/list would leak the full tool schema
        // (arg names, descriptions) to anyone who can reach the port —
        // useful reconnaissance for an attacker even when they cannot
        // invoke the tools.
        "tools/list" => match caller {
            Some(_) => Ok(tools_list_response()),
            None => Err(McpError::unauthorized()),
        },
        "tools/call" => match caller {
            Some(ref c) => handle_tools_call(&hub, c, params).await,
            None => Err(McpError::unauthorized()),
        },
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

    /// Returned when `tools/list` or `tools/call` is invoked without a
    /// valid bearer token.
    fn unauthorized() -> Self {
        Self {
            code: -32600, // Invalid Request
            message: "unauthorized: valid bearer token required".into(),
        }
    }
}

/// Definitions for `tools/list`.
fn tools_list_response() -> Value {
    json!({
        "tools": [
            {
                "name": "list_nodes",
                "description": "List every node registered with the swarm hub, with \
                    full hardware (CPU/GPU/RAM/disk), OS, capabilities, status, busy \
                    task_id (when busy), and last heartbeat timestamp. Call this \
                    BEFORE swarm_dispatch/swarm_submit so you can pick a target \
                    node_id that genuinely fits the task — the LLM caller has much \
                    more context than the hub's blunt constraint filter.",
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
                "description": "Dispatch a task to a specific node and block on the result. \
                    Best for short tasks (under the dispatch timeout). For long-running work \
                    like model fine-tuning, prefer swarm_submit.\n\
                    \n\
                    PREFERRED FLOW: call list_nodes, reason about which node best fits \
                    this task (hardware, capabilities, OS, current status, whether it \
                    recently ran a related task), and pass its `node_id` as \
                    `target_node_id`. If you genuinely don't care which node runs it, \
                    pass `constraints` instead as a shortcut. Exactly one of \
                    `target_node_id` or `constraints` must be provided. On \
                    `NodeNotIdle` or `NodeNotFound`, re-call list_nodes and try another.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string" },
                        "payloads": { "type": "array" },
                        "timeout_secs": { "type": "integer" },
                        "target_node_id": {
                            "type": "string",
                            "description": "Preferred path. Explicit node_id from list_nodes. \
                                Fails with NodeNotFound if unknown or NodeNotIdle if the \
                                target is busy/draining."
                        },
                        "constraints": {
                            "type": "object",
                            "description": "Shortcut path. Hub picks an idle node matching \
                                these filters (most-free-RAM first). Use only when any \
                                matching node is fine.",
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
                "description": "Dispatch a long-running task to a specific node and return \
                    a task_id immediately. Use this for model fine-tuning, large batch jobs, \
                    or anything that may run for minutes to hours. Poll swarm_task_status and \
                    swarm_task_checkpoints for progress, and swarm_task_result for the final \
                    SwarmResult once the task reaches a terminal state.\n\
                    \n\
                    PREFERRED FLOW: call list_nodes, reason about which node best fits \
                    this task (hardware, capabilities, OS, current status), and pass its \
                    `node_id` as `target_node_id`. If you genuinely don't care which node \
                    runs it, pass `constraints` instead as a shortcut. Exactly one of \
                    `target_node_id` or `constraints` must be provided. On `NodeNotIdle` \
                    or `NodeNotFound`, re-call list_nodes and try another.",
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
                        "target_node_id": {
                            "type": "string",
                            "description": "Preferred path. Explicit node_id from list_nodes. \
                                Fails with NodeNotFound if unknown or NodeNotIdle if the \
                                target is busy/draining."
                        },
                        "constraints": {
                            "type": "object",
                            "description": "Shortcut path. Hub picks an idle node matching \
                                these filters (most-free-RAM first). Use only when any \
                                matching node is fine.",
                            "properties": {
                                "needs_gpu": { "type": "boolean" },
                                "needs_capability": { "type": "string" },
                                "min_ram_gb": { "type": "integer" }
                            },
                            "additionalProperties": false
                        },
                        "idempotency_key": {
                            "type": "string",
                            "maxLength": 256,
                            "description": "Optional caller-chosen key for retry-safe \
                                submission. A repeat swarm_submit with the same key from \
                                the same caller returns the original task_id and sets \
                                `idempotent_replay: true` on the response instead of \
                                dispatching a new task. Mappings live for 24h."
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
                "name": "swarm_task_cancel",
                "description": "Request cancellation of a running task. The hub marks the \
                    task as cancelled and pushes a cancel_task event to the owning node, \
                    which drops the in-flight agent run. Bash subprocesses spawned by \
                    the node may continue until their current tool call yields, so \
                    cancellation is cooperative rather than instant.",
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
                "description": "List your recent tasks, newest first, bounded by limit.",
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
///
/// Every tool handler returns a `Result<Value, String>`.  Ok values are
/// rendered as pretty-printed JSON text content; Err strings become
/// error text content with `isError: true`.  This keeps the match arms
/// trivial one-liners.
async fn handle_tools_call(
    hub: &Arc<Hub>,
    caller: &McpCaller,
    params: Option<Value>,
) -> Result<Value, McpError> {
    let params = params.ok_or_else(|| McpError::invalid_params("missing params"))?;
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| McpError::invalid_params("params.name is required"))?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    let outcome: Result<Value, String> = match name {
        "list_nodes" => Ok(list_nodes(hub, caller).await),
        "swarm_status" => Ok(swarm_status(hub).await),
        "swarm_dispatch" => swarm_dispatch(hub, caller, arguments)
            .await
            .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
            .map_err(|e| format!("dispatch failed: {e}")),
        "swarm_submit" => swarm_submit(hub, caller, arguments)
            .await
            .map_err(|e| format!("submit failed: {e}")),
        "swarm_task_status" => swarm_task_status(hub, caller, arguments).await,
        "swarm_task_checkpoints" => swarm_task_checkpoints(hub, caller, arguments).await,
        "swarm_task_result" => swarm_task_result(hub, caller, arguments).await,
        "swarm_task_cancel" => swarm_task_cancel(hub, caller, arguments).await,
        "swarm_task_list" => Ok(swarm_task_list(hub, caller, arguments).await),
        other => {
            return Err(McpError::invalid_params(format!("unknown tool: {other}")));
        }
    };

    Ok(tool_result(outcome))
}

/// Render a handler outcome as an MCP `tools/call` result block.
fn tool_result(outcome: Result<Value, String>) -> Value {
    match outcome {
        Ok(v) => {
            let text =
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            })
        }
        Err(e) => json!({
            "content": [{ "type": "text", "text": e }],
            "isError": true,
        }),
    }
}

/// `list_nodes` — registered nodes, excluding `caller` if set.
///
/// Output is deliberately verbose so an LLM caller has enough context
/// to pick a target node for `swarm_dispatch` / `swarm_submit` without
/// guessing. Full CPU/GPU lists, disk_free_bytes, OS, the in-flight
/// `task_id` (when busy), and a Unix heartbeat timestamp are all
/// included.
async fn list_nodes(hub: &Arc<Hub>, caller: &McpCaller) -> Value {
    let exclude = caller.node_name.as_deref();
    hub.registry
        .with_entries(|entries| {
            // Pre-size the row vector so we don't grow-and-memcpy.
            let mut rows: Vec<Value> = Vec::with_capacity(entries.len());
            for entry in entries.values() {
                if exclude.is_some_and(|c| entry.manifest.node_name == c) {
                    continue;
                }
                let hw = &entry.manifest.hardware;
                let last_heartbeat_unix = entry
                    .last_heartbeat_at
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                // Build the object in one shot so optional fields are
                // inline — no second pass through `.as_object_mut()`
                // and no `.unwrap()` on a macro invariant.
                let mut row = serde_json::Map::with_capacity(10);
                row.insert("node_id".into(), Value::String(entry.node_id.clone()));
                row.insert(
                    "node_name".into(),
                    Value::String(entry.manifest.node_name.clone()),
                );
                row.insert("os".into(), Value::String(entry.manifest.os.clone()));
                row.insert("status".into(), Value::from(status_label(&entry.status)));
                row.insert(
                    "capabilities".into(),
                    serde_json::to_value(&entry.manifest.capabilities)
                        .unwrap_or(Value::Null),
                );
                row.insert(
                    "hardware".into(),
                    json!({
                        "ram_bytes": hw.ram_bytes,
                        "disk_free_bytes": hw.disk_free_bytes,
                        "cpus": hw.cpus,
                        "gpus": hw.gpus,
                    }),
                );
                row.insert("last_heartbeat_unix".into(), Value::from(last_heartbeat_unix));
                if let Some(desc) = &entry.manifest.description {
                    row.insert("description".into(), Value::String(desc.clone()));
                }
                if let NodeStatus::Busy { task_id } = &entry.status {
                    row.insert("busy_task_id".into(), Value::String(task_id.clone()));
                }
                rows.push(Value::Object(row));
            }
            // Stable order by node_id.  Unwrap is safe: every row above
            // was constructed with a String node_id.
            rows.sort_by(|a, b| {
                a.get("node_id").and_then(Value::as_str).unwrap_or("").cmp(
                    b.get("node_id").and_then(Value::as_str).unwrap_or(""),
                )
            });
            Value::Array(rows)
        })
        .await
}

const fn status_label(status: &NodeStatus) -> &'static str {
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

/// How a dispatch caller selected the target node.
///
/// This is the heart of the "caller-directed routing" refactor:
/// dispatch tools now take either `target_node_id` (preferred — the
/// LLM has reasoned over `list_nodes` and picked) or `constraints`
/// (the legacy three-field filter, kept as a shortcut for callers
/// that genuinely don't care which node runs the task). Exactly one
/// must be provided.
enum DispatchTarget {
    Explicit(NodeId),
    Constraints(RoutingConstraints),
}

/// Fields parsed out of a dispatch/submit tool-call's arguments.
struct DispatchArgs {
    prompt: String,
    payloads: Vec<Payload>,
    timeout_secs: Option<u64>,
    target: DispatchTarget,
    /// Optional caller-supplied key.  Only honored on `swarm_submit`.
    /// A repeat submit with the same (caller, key) returns the original
    /// task_id instead of dispatching a new task.
    idempotency_key: Option<String>,
}

fn parse_dispatch_args(arguments: Value) -> Result<DispatchArgs, DispatchError> {
    let prompt = arguments
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| DispatchError::InvalidArgs("missing prompt".into()))?
        .to_string();

    let payloads: Vec<Payload> = match arguments.get("payloads") {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone())
            .map_err(|e| DispatchError::InvalidArgs(format!("invalid payloads: {e}")))?,
        _ => vec![],
    };

    let timeout_secs = arguments.get("timeout_secs").and_then(serde_json::Value::as_u64);

    let idempotency_key = arguments
        .get("idempotency_key")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(std::string::ToString::to_string);
    if let Some(ref k) = idempotency_key
        && k.len() > crate::idempotency::MAX_KEY_LEN
    {
        return Err(DispatchError::InvalidArgs(format!(
            "idempotency_key too long ({} chars, max {})",
            k.len(),
            crate::idempotency::MAX_KEY_LEN,
        )));
    }

    let target = parse_target(&arguments)?;

    Ok(DispatchArgs {
        prompt,
        payloads,
        timeout_secs,
        target,
        idempotency_key,
    })
}

/// Pick the `DispatchTarget` out of a tool-call's arguments.
///
/// Rules:
/// - `target_node_id` (non-empty string) → `Explicit`.
/// - `constraints` object present (even empty `{}`) → `Constraints`.
/// - Both present → error (mutually exclusive).
/// - Neither present → `NoTargetOrConstraints`.
fn parse_target(arguments: &Value) -> Result<DispatchTarget, DispatchError> {
    let explicit = arguments
        .get("target_node_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(std::string::ToString::to_string);

    let constraints_present = arguments
        .get("constraints")
        .is_some_and(|v| !v.is_null());

    match (explicit, constraints_present) {
        (Some(_), true) => Err(DispatchError::InvalidArgs(
            "target_node_id and constraints are mutually exclusive".into(),
        )),
        (Some(id), false) => Ok(DispatchTarget::Explicit(id)),
        (None, true) => Ok(DispatchTarget::Constraints(parse_constraints(arguments)?)),
        (None, false) => Err(DispatchError::NoTargetOrConstraints),
    }
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
    owner: Option<&str>,
    waiter: Option<oneshot::Sender<SwarmResult>>,
    reserved_task_id: Option<String>,
) -> Result<PlacedTask, DispatchError> {
    let node_id = match &args.target {
        DispatchTarget::Explicit(id) => select_node_by_id(&hub.registry, id).await?,
        DispatchTarget::Constraints(c) => select_node(&hub.registry, c)
            .await
            .ok_or(DispatchError::NoEligibleNode)?,
    };

    let task = SwarmTask {
        task_id: reserved_task_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        prompt: args.prompt.clone(),
        payloads: args.payloads.clone(),
        timeout_secs: args.timeout_secs,
    };

    let canonical = serde_json::to_vec(&task)
        .map_err(|e| DispatchError::Transient(format!("task serialization failed: {e}")))?;
    let wire = hub.key.sign_task(&canonical);

    let submitted_at = SystemTime::now();
    let record = TaskRecord {
        task_id: task.task_id.clone(),
        node_id: node_id.clone(),
        owner: owner.map(str::to_string),
        prompt_preview: truncate_prompt(&args.prompt),
        submitted_at,
        last_update: submitted_at,
        state: TaskState::Running,
        checkpoints: Vec::new(),
        result: None,
        waiter,
    };
    if let Err(e) = hub.tasks.insert(record).await {
        tracing::error!(task_id = %task.task_id, error = %e, "failed to persist task insert");
        return Err(DispatchError::Transient(format!(
            "failed to persist task: {e}"
        )));
    }

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
        // observe the DispatchError::Transient return value instead.
        let _ = hub.tasks.finalize(&task.task_id, failed).await;
        hub.registry.set_status(&node_id, NodeStatus::Idle).await;
        return Err(DispatchError::Transient(format!(
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

/// Cap `prompt` at roughly `PROMPT_PREVIEW_CHARS` bytes for storage on the
/// `TaskRecord`, appending `"..."` when we actually shorten the string.
///
/// Prompts up to `PROMPT_PREVIEW_CHARS` bytes are returned unchanged;
/// longer prompts are sliced on a UTF-8 char boundary (so we never
/// split a code point) and get an ellipsis suffix to signal truncation
/// to downstream tools.
fn truncate_prompt(prompt: &str) -> String {
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
async fn swarm_dispatch(
    hub: &Arc<Hub>,
    caller: &McpCaller,
    arguments: Value,
) -> Result<SwarmResult, DispatchError> {
    let args = parse_dispatch_args(arguments)?;
    let timeout = args
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_DISPATCH_TIMEOUT);

    let (tx, rx) = oneshot::channel::<SwarmResult>();
    let placed = place_task(hub, &args, Some(&caller.node_id), Some(tx), None).await?;

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
            Err(DispatchError::Transient("result channel closed".into()))
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
///
/// Honors `idempotency_key`: a retry with the same key and caller
/// returns the original `task_id` without dispatching a new task.
async fn swarm_submit(
    hub: &Arc<Hub>,
    caller: &McpCaller,
    arguments: Value,
) -> Result<Value, DispatchError> {
    let args = parse_dispatch_args(arguments)?;

    // Pre-generate the task_id so the idempotency record points at the
    // real task we're about to place.  If a *committed* duplicate exists
    // under this key, short-circuit with the original id.  Otherwise
    // hold a tentative reservation that we commit only after `place_task`
    // succeeds — a failed dispatch rolls the reservation back so a retry
    // isn't pinned to a phantom task_id.
    let reserved_task_id = uuid::Uuid::new_v4().to_string();
    let (effective_task_id, reservation) =
        if let Some(key) = args.idempotency_key.as_deref() {
            match hub
                .idempotency
                .reserve(&caller.node_id, key, &reserved_task_id)
                .await
            {
                crate::idempotency::Reservation::Replay(existing) => {
                    // Replay: return the original task_id without a fresh dispatch.
                    let snap = hub.tasks.get_owned(&existing, &caller.node_id).await;
                    let (node_id, submitted_at_unix, state) = match snap {
                        Some(s) => (
                            s.node_id,
                            s.submitted_at_unix,
                            format!("{:?}", s.state).to_ascii_lowercase(),
                        ),
                        None => (String::new(), 0, "unknown".into()),
                    };
                    return Ok(json!({
                        "task_id": existing,
                        "node_id": node_id,
                        "submitted_at_unix": submitted_at_unix,
                        "state": state,
                        "idempotent_replay": true,
                    }));
                }
                crate::idempotency::Reservation::Fresh(handle) => {
                    (reserved_task_id, Some(handle))
                }
            }
        } else {
            (reserved_task_id, None)
        };

    let placed = match place_task(
        hub,
        &args,
        Some(&caller.node_id),
        None,
        Some(effective_task_id),
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            // Dispatch failed — drop the tentative reservation so the
            // caller can retry with the same idempotency key.
            if let Some(h) = reservation {
                hub.idempotency.rollback(h).await;
            }
            return Err(e);
        }
    };

    // Task is placed in the store; promote the reservation to committed.
    if let Some(h) = reservation {
        hub.idempotency.commit(h).await;
    }

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
        .map(std::string::ToString::to_string)
        .ok_or_else(|| "task_id is required".to_string())
}

/// Fetch a task snapshot, scoped to the caller's verified identity.
/// Returns "unknown task_id" for both nonexistent and non-owned tasks
/// (topology obfuscation).
async fn scoped_get(
    hub: &Arc<Hub>,
    caller: &McpCaller,
    task_id: &str,
) -> Result<TaskSnapshot, String> {
    hub.tasks
        .get_owned(task_id, &caller.node_id)
        .await
        .ok_or_else(|| format!("unknown task_id: {task_id}"))
}

async fn swarm_task_status(
    hub: &Arc<Hub>,
    caller: &McpCaller,
    arguments: Value,
) -> Result<Value, String> {
    let task_id = required_task_id(&arguments)?;
    let snap = scoped_get(hub, caller, &task_id).await?;
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

async fn swarm_task_checkpoints(
    hub: &Arc<Hub>,
    caller: &McpCaller,
    arguments: Value,
) -> Result<Value, String> {
    let task_id = required_task_id(&arguments)?;
    let since = arguments
        .get("since_sequence")
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(0);
    let cps = hub
        .tasks
        .checkpoints_since_owned(&task_id, since, &caller.node_id)
        .await
        .ok_or_else(|| format!("unknown task_id: {task_id}"))?;
    Ok(json!({
        "task_id": task_id,
        "since_sequence": since,
        "checkpoints": cps,
    }))
}

/// `swarm_task_cancel` — mark a running task Cancelled and push a
/// cancel_task SSE event to the owning node.
async fn swarm_task_cancel(
    hub: &Arc<Hub>,
    caller: &McpCaller,
    arguments: Value,
) -> Result<Value, String> {
    let task_id = required_task_id(&arguments)?;

    let Some((node_id, waiter)) = hub.tasks.cancel_owned(&task_id, &caller.node_id).await else {
        return Err(format!(
            "task '{task_id}' is unknown or already terminal"
        ));
    };

    // Wake any sync dispatcher that's still blocked on this task so
    // swarm_dispatch returns promptly with a Cancelled result.
    if let Some(tx) = waiter {
        let cancelled = dyson_swarm_protocol::types::SwarmResult {
            task_id: task_id.clone(),
            text: String::new(),
            payloads: vec![],
            status: dyson_swarm_protocol::types::TaskStatus::Cancelled,
            duration_secs: 0,
        };
        let _ = tx.send(cancelled);
    }

    // Flip the node back to Idle optimistically — it will confirm via
    // its next heartbeat.  The cancel event below tells the node to
    // drop the in-flight work.
    hub.registry.set_status(&node_id, NodeStatus::Idle).await;

    let pushed = hub
        .registry
        .push_event(&node_id, SseEvent::CancelTask(task_id.clone()))
        .await;

    tracing::info!(
        task_id = %task_id,
        node_id = %node_id,
        pushed,
        "cancelled task"
    );

    Ok(json!({
        "task_id": task_id,
        "node_id": node_id,
        "state": "cancelled",
        "event_delivered": pushed,
    }))
}

async fn swarm_task_result(
    hub: &Arc<Hub>,
    caller: &McpCaller,
    arguments: Value,
) -> Result<Value, String> {
    let task_id = required_task_id(&arguments)?;
    let snap = scoped_get(hub, caller, &task_id).await?;
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

async fn swarm_task_list(hub: &Arc<Hub>, caller: &McpCaller, arguments: Value) -> Value {
    let limit = arguments
        .get("limit")
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_LIST_LIMIT);
    let snaps = hub.tasks.list_owned(&caller.node_id, limit).await;
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
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let needs_capability = obj
        .get("needs_capability")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let min_ram_gb = obj.get("min_ram_gb").and_then(serde_json::Value::as_u64);

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
        let store = crate::tasks::TaskStore::new_for_test().await;
        store
            .insert(crate::tasks::TaskRecord {
                task_id: "t1".into(),
                node_id: "node-a".into(),
                owner: None,
                prompt_preview: "do a thing".into(),
                submitted_at: SystemTime::now(),
                last_update: SystemTime::now(),
                state: TaskState::Running,
                checkpoints: vec![],
                result: None,
                waiter: None,
            })
            .await
            .unwrap();
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
            "constraints": {},
        });
        let parsed = parse_dispatch_args(args).unwrap();
        assert!(parsed.payloads.is_empty());
    }

    #[test]
    fn parse_target_explicit_node_id() {
        let args = json!({
            "prompt": "hi",
            "target_node_id": "abc-123",
        });
        let parsed = parse_dispatch_args(args).unwrap();
        match parsed.target {
            DispatchTarget::Explicit(id) => assert_eq!(id, "abc-123"),
            DispatchTarget::Constraints(_) => panic!("expected Explicit target"),
        }
    }

    #[test]
    fn parse_target_empty_constraints_is_allowed_shortcut() {
        let args = json!({
            "prompt": "hi",
            "constraints": {},
        });
        let parsed = parse_dispatch_args(args).unwrap();
        match parsed.target {
            DispatchTarget::Constraints(c) => {
                assert!(!c.needs_gpu);
                assert!(c.needs_capability.is_none());
                assert!(c.min_ram_gb.is_none());
            }
            DispatchTarget::Explicit(_) => panic!("expected Constraints target"),
        }
    }

    #[test]
    fn parse_target_both_is_rejected_as_mutually_exclusive() {
        let args = json!({
            "prompt": "hi",
            "target_node_id": "abc-123",
            "constraints": { "needs_gpu": true },
        });
        match parse_dispatch_args(args) {
            Err(DispatchError::InvalidArgs(m)) => {
                assert!(
                    m.contains("mutually exclusive"),
                    "wrong InvalidArgs message: {m}"
                );
            }
            Err(other) => panic!("expected InvalidArgs, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn parse_target_neither_is_rejected() {
        let args = json!({ "prompt": "hi" });
        match parse_dispatch_args(args) {
            Err(DispatchError::NoTargetOrConstraints) => {}
            Err(other) => panic!("expected NoTargetOrConstraints, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn parse_target_empty_string_falls_back_to_constraints_rule() {
        // target_node_id of "" is treated as absent; with no constraints
        // key present either, we expect NoTargetOrConstraints.
        let args = json!({
            "prompt": "hi",
            "target_node_id": "",
        });
        match parse_dispatch_args(args) {
            Err(DispatchError::NoTargetOrConstraints) => {}
            Err(other) => panic!("expected NoTargetOrConstraints, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }
}
