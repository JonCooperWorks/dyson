//! `POST /swarm/task/{task_id}/progress` — workers report progress on
//! long-running tasks.
//!
//! The body is a [`ProgressReport`] with optional progress fraction,
//! human-readable message, and a log chunk to append. The handler updates
//! the scheduler's task store, sliding the task back from `stalled` to
//! `running` if needed.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

use crate::Hub;
use crate::auth::AuthedNode;
use crate::scheduler::ProgressReport;

pub async fn progress_handler(
    State(hub): State<Arc<Hub>>,
    AuthedNode(_node_id): AuthedNode,
    Path(task_id): Path<String>,
    Json(report): Json<ProgressReport>,
) -> StatusCode {
    if report.task_id != task_id {
        tracing::warn!(
            url_id = %task_id,
            body_id = %report.task_id,
            "progress: task_id mismatch"
        );
        return StatusCode::BAD_REQUEST;
    }

    match hub
        .tasks
        .record_progress(
            &task_id,
            report.progress,
            report.message.as_deref(),
            report.log.as_deref(),
        )
        .await
    {
        Ok(()) => StatusCode::OK,
        Err(e) => {
            tracing::warn!(%task_id, error = %e, "progress: store update failed");
            StatusCode::NOT_FOUND
        }
    }
}
