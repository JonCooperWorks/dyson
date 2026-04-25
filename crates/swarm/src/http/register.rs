//! `POST /swarm/register` — accept a node manifest, issue a bearer token.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
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
    headers: HeaderMap,
    Json(manifest): Json<NodeManifest>,
) -> Response {
    // When an API key is configured, /swarm/register is gated behind
    // it.  When unset (loopback bind or --dangerous-no-auth) the
    // endpoint stays open so node bootstrapping works without any
    // shared secret.
    if let Some(key) = hub.mcp_api_key.as_ref() {
        let ok = crate::auth::extract_bearer(&headers)
            .is_some_and(|t| key.verify(&t));
        if !ok {
            return (
                StatusCode::UNAUTHORIZED,
                "valid MCP API key required for registration",
            )
                .into_response();
        }
    }

    match hub.registry.register(manifest).await {
        Ok((node_id, token)) => {
            tracing::info!(node_id = %node_id, "node registered");
            Json(RegisterResponse { node_id, token }).into_response()
        }
        Err(e) => {
            // Persist-first means we return without having mutated any
            // in-memory state; the caller can safely retry.
            tracing::error!(error = %e, "failed to persist node register");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "failed to persist registration",
            )
                .into_response()
        }
    }
}
