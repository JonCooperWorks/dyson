//! `POST /swarm/register` — accept a node manifest, issue a bearer token.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use dyson_swarm_protocol::types::NodeManifest;
use serde::Serialize;

use crate::Hub;

/// Body of `POST /swarm/register`'s response.
///
/// This must match `crates/dyson/src/swarm/connection.rs::RegisterResponse`
/// byte-for-byte — the node deserializes this.
#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub node_id: String,
    pub token: String,
}

pub async fn register_handler(
    State(hub): State<Arc<Hub>>,
    Json(manifest): Json<NodeManifest>,
) -> Json<RegisterResponse> {
    let (node_id, token) = hub.registry.register(manifest).await;
    tracing::info!(node_id = %node_id, "node registered");
    Json(RegisterResponse { node_id, token })
}
