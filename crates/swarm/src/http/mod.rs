//! HTTP surface — axum Router wiring for every hub endpoint.
//!
//! The handlers themselves live in sibling modules:
//!
//! - `register` — `POST /swarm/register`
//! - `events`   — `GET  /swarm/events`         (SSE)
//! - `heartbeat`— `POST /swarm/heartbeat`
//! - `result`   — `POST /swarm/result`
//! - `blob`     — `GET/PUT /swarm/blob/{sha}`
//! - `mcp`      — `POST /mcp`

pub mod blob;
pub mod events;
pub mod heartbeat;
pub mod mcp;
pub mod progress;
pub mod register;
pub mod result;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

use crate::Hub;

/// Build the full HTTP router for the hub.
pub fn build_router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/swarm/register", post(register::register_handler))
        .route("/swarm/events", get(events::events_handler))
        .route("/swarm/heartbeat", post(heartbeat::heartbeat_handler))
        .route("/swarm/result", post(result::result_handler))
        .route(
            "/swarm/task/:task_id/progress",
            post(progress::progress_handler),
        )
        .route(
            "/swarm/blob/:sha256",
            get(blob::get_blob_handler).put(blob::put_blob_handler),
        )
        .route("/mcp", post(mcp::mcp_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(hub)
}
