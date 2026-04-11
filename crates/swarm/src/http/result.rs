//! `POST /swarm/result` — delivery of task results from nodes back to the
//! hub.
//!
//! The handler does three things:
//!
//! 1. Flip the worker's registry status back to `Idle`.
//! 2. Persist the terminal state to the scheduler's task store and fire
//!    the notifier (if there are notification channels for this task).
//! 3. If a synchronous caller is waiting on this task (legacy
//!    `swarm_dispatch` flow), wake them via the pending oneshot.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use dyson_swarm_protocol::types::{NodeStatus, SwarmResult, TaskStatus};

use crate::Hub;
use crate::auth::AuthedNode;
use crate::scheduler::TerminalStatus;

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

    // Persist to the durable task store. Tasks that came in via the
    // legacy `swarm_dispatch` path were never inserted, so a
    // not-found here is benign — we just skip the durable side.
    let task_known = matches!(hub.tasks.get(&result.task_id).await, Ok(Some(_)));
    if task_known {
        let terminal = match &result.status {
            TaskStatus::Completed => TerminalStatus::Completed,
            TaskStatus::Failed { error } => TerminalStatus::Failed {
                error: error.clone(),
            },
            TaskStatus::Cancelled => TerminalStatus::Cancelled { reason: None },
        };
        if let Err(e) = hub
            .tasks
            .finish(
                &result.task_id,
                terminal,
                Some(result.text.clone()),
                Some(&result.payloads),
                result.duration_secs,
            )
            .await
        {
            tracing::warn!(task_id = %result.task_id, error = %e, "task store finish failed");
        } else {
            // Fire the notifier (best-effort, async).
            hub.notifier.notify(result.task_id.clone()).await;
        }
    }

    // Hand the result off to whoever is awaiting it (legacy synchronous
    // dispatch).
    let tx_opt = {
        let mut pending = hub.pending_dispatches.lock().await;
        pending.remove(&result.task_id)
    };

    if let Some(tx) = tx_opt {
        // If send fails, the MCP caller hung up — log but still 200.
        let _ = tx.send(result);
    } else if !task_known {
        tracing::warn!(
            task_id = %result.task_id,
            "result for unknown task — dropping"
        );
    }

    StatusCode::OK
}
