//! `POST /swarm/result` — delivery of task results from nodes back to the
//! MCP caller that dispatched them.
//!
//! Every result flows through `TaskStore::finalize`, which updates the
//! stored record and returns any waiting oneshot sender (only set for
//! sync `swarm_dispatch` callers).  Async `swarm_submit` callers don't
//! block — they discover the result by polling `swarm_task_result`.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use dyson_swarm_protocol::types::{NodeStatus, SwarmResult};

use crate::Hub;
use crate::auth::AuthedNode;

pub async fn result_handler(
    State(hub): State<Arc<Hub>>,
    AuthedNode(node_id): AuthedNode,
    Json(result): Json<SwarmResult>,
) -> StatusCode {
    tracing::info!(
        node_id = %node_id,
        task_id = %result.task_id,
        "received task result"
    );

    // Flip the node back to Idle so the router will pick it again.
    hub.registry.set_status(&node_id, NodeStatus::Idle).await;

    // Record the terminal state and retrieve any sync waiter.  The
    // TaskStore returns `None` for async submissions, for unknown tasks,
    // and for tasks whose waiter was previously abandoned on timeout.
    let waiter = hub.tasks.finalize(&result.task_id, result.clone()).await;

    match waiter {
        Some(tx) => {
            // Fire the oneshot outside any lock.  Send failure just
            // means the MCP caller already hung up — log but still 200.
            if tx.send(result).is_err() {
                tracing::debug!("sync dispatcher already hung up");
            }
        }
        None => {
            // Async task, or sync task whose waiter was cleared, or
            // truly unknown.  `TaskStore::finalize` already logged the
            // unknown case implicitly by returning None for a missing
            // record; surface it here for operator visibility.
            tracing::debug!(
                task_id = %result.task_id,
                "no waiter for result (async submission or unknown task)"
            );
        }
    }

    StatusCode::OK
}
