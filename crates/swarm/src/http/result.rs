//! `POST /swarm/result` — delivery of task results from nodes back to the
//! MCP caller that dispatched them.

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
    hub.registry
        .set_status(&node_id, NodeStatus::Idle)
        .await;

    // Hand the result off to whoever is awaiting it.
    let tx_opt = {
        let mut pending = hub.pending_dispatches.lock().await;
        pending.remove(&result.task_id)
    };

    if let Some(tx) = tx_opt {
        // If send fails, the MCP caller hung up — log but still 200.
        let _ = tx.send(result);
    } else {
        tracing::warn!(
            task_id = %result.task_id,
            "result for unknown task — dropping"
        );
    }

    StatusCode::OK
}
