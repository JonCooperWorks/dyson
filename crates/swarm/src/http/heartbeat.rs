//! `POST /swarm/heartbeat` — nodes tell us they're alive.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use dyson_swarm_protocol::types::NodeStatus;

use crate::Hub;
use crate::auth::AuthedNode;
use crate::registry::SseEvent;

pub async fn heartbeat_handler(
    State(hub): State<Arc<Hub>>,
    AuthedNode(node_id): AuthedNode,
    Json(status): Json<NodeStatus>,
) -> StatusCode {
    if !hub.registry.heartbeat(&node_id, status).await {
        return StatusCode::UNAUTHORIZED;
    }
    // Push an ack into the SSE stream as documented in docs/swarm.md.
    hub.registry
        .push_event(&node_id, SseEvent::HeartbeatAck)
        .await;
    StatusCode::OK
}
