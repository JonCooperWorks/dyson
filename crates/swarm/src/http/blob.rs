//! `GET/PUT /swarm/blob/{sha256}` — content-addressed blob transfer.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::Hub;
use crate::auth::AuthedNode;
use crate::blob::BlobError;

pub async fn get_blob_handler(
    State(hub): State<Arc<Hub>>,
    // Require auth to pull blobs — the node is already authed, and this
    // prevents random HTTP scrapers from walking the content-addressed store.
    _node: AuthedNode,
    Path(sha256): Path<String>,
) -> Response {
    match hub.blobs.get(&sha256).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(BlobError::NotFound(_)) => (StatusCode::NOT_FOUND, "blob not found").into_response(),
        Err(BlobError::InvalidHash(_)) => {
            (StatusCode::BAD_REQUEST, "invalid sha256 path").into_response()
        }
        Err(BlobError::HashMismatch { .. }) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "disk corruption: stored blob hash mismatch",
        )
            .into_response(),
        Err(BlobError::Io(e)) => {
            tracing::error!(error = %e, sha256 = %sha256, "blob read failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "io error").into_response()
        }
    }
}

pub async fn put_blob_handler(
    State(hub): State<Arc<Hub>>,
    _node: AuthedNode,
    Path(sha256): Path<String>,
    body: Bytes,
) -> Response {
    match hub.blobs.put(&sha256, &body).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(BlobError::HashMismatch { expected, got }) => (
            StatusCode::BAD_REQUEST,
            format!("hash mismatch: expected {expected}, got {got}"),
        )
            .into_response(),
        Err(BlobError::InvalidHash(s)) => {
            (StatusCode::BAD_REQUEST, format!("invalid sha256: {s}")).into_response()
        }
        Err(BlobError::NotFound(_)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "unexpected not-found on put").into_response()
        }
        Err(BlobError::Io(e)) => {
            tracing::error!(error = %e, sha256 = %sha256, "blob write failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "io error").into_response()
        }
    }
}
