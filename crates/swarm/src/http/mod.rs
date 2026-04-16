//! HTTP surface — axum Router wiring for every hub endpoint.
//!
//! The handlers themselves live in sibling modules:
//!
//! - `register`   — `POST /swarm/register`
//! - `events`     — `GET  /swarm/events`         (SSE)
//! - `heartbeat`  — `POST /swarm/heartbeat`
//! - `result`     — `POST /swarm/result`
//! - `checkpoint` — `POST /swarm/checkpoint`
//! - `blob`       — `GET/PUT /swarm/blob/{sha}`
//! - `mcp`        — `POST /mcp`

pub mod blob;
pub mod checkpoint;
pub mod events;
pub mod heartbeat;
pub mod mcp;
pub mod register;
pub mod result;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::Hub;

// Per-endpoint body size limits.  Axum's default DefaultBodyLimit is 2 MiB,
// but we set these explicitly so the surface is obvious at the router layer
// rather than buried in Axum's defaults.
//
// Dispatch/MCP messages are JSON with inline payloads — an LLM prompt plus
// tool results.  Very large prompts should use the blob store, so 4 MiB is
// a generous ceiling that still bounds memory per request.
const MCP_BODY_LIMIT: usize = 4 * 1024 * 1024;
/// Blob store upload cap.  Sized at 16 MiB — large enough for meaningful
/// artefacts (model state shards, packed conversation transcripts) while
/// keeping per-upload memory pressure bounded.  Operators who need more
/// should run blobs through object storage, not the hub.
const BLOB_BODY_LIMIT: usize = 16 * 1024 * 1024;
/// Small-message endpoints (register, heartbeat, result, checkpoint).
/// These carry a handful of fields plus optional checkpoint payloads —
/// generous 256 KiB keeps obvious DoS bodies out without restricting use.
const CONTROL_BODY_LIMIT: usize = 256 * 1024;

/// Build the full HTTP router for the hub.
pub fn build_router(hub: Arc<Hub>) -> Router {
    // Control-plane endpoints: small JSON bodies only.
    let control = Router::new()
        .route("/swarm/register", post(register::register_handler))
        .route("/swarm/heartbeat", post(heartbeat::heartbeat_handler))
        .route("/swarm/result", post(result::result_handler))
        .route("/swarm/checkpoint", post(checkpoint::checkpoint_handler))
        .layer(RequestBodyLimitLayer::new(CONTROL_BODY_LIMIT));

    // Blob store: higher cap, verified by SHA-256 at write time.
    let blobs = Router::new()
        .route(
            "/swarm/blob/:sha256",
            get(blob::get_blob_handler).put(blob::put_blob_handler),
        )
        .layer(RequestBodyLimitLayer::new(BLOB_BODY_LIMIT));

    // MCP endpoint: dispatch envelopes with inline payloads.
    let mcp = Router::new()
        .route("/mcp", post(mcp::mcp_handler))
        .layer(RequestBodyLimitLayer::new(MCP_BODY_LIMIT));

    // Events: GET-only SSE stream, no request body to limit.
    let events =
        Router::new().route("/swarm/events", get(events::events_handler));

    control
        .merge(blobs)
        .merge(mcp)
        .merge(events)
        .layer(TraceLayer::new_for_http())
        .with_state(hub)
}
