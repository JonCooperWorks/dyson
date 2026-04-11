//! `POST /swarm/checkpoint` — progress updates from a running task.
//!
//! Nodes POST one of these whenever the agent emits a `swarm_checkpoint`
//! tool call during task execution.  The hub appends it to the task's
//! record so callers polling `swarm_task_status` or
//! `swarm_task_checkpoints` can observe progress without waiting for the
//! final `SwarmResult`.
//!
//! Authed via the node's bearer token so arbitrary clients can't spoof
//! progress for another node's task.  We trust the node to only submit
//! checkpoints for tasks it is currently executing.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use dyson_swarm_protocol::types::TaskCheckpoint;

use crate::Hub;
use crate::auth::AuthedNode;

pub async fn checkpoint_handler(
    State(hub): State<Arc<Hub>>,
    AuthedNode(node_id): AuthedNode,
    Json(cp): Json<TaskCheckpoint>,
) -> StatusCode {
    tracing::info!(
        node_id = %node_id,
        task_id = %cp.task_id,
        sequence = cp.sequence,
        progress = ?cp.progress,
        "received task checkpoint"
    );

    if hub.tasks.append_checkpoint(cp).await {
        StatusCode::OK
    } else {
        // Either the task is unknown or it's already terminal (late
        // checkpoint after a final result landed).  Both cases are
        // reported the same way.
        StatusCode::NOT_FOUND
    }
}
