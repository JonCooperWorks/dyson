//! Task queue — placeholder for v2.
//!
//! In v1, the hub has no pending queue: if no node is idle when a
//! dispatch request arrives, `swarm_dispatch` returns an error
//! immediately.  This module exists so v2 has a natural home.

use thiserror::Error;

/// Errors produced by the dispatcher when it can't place a task.
#[derive(Debug, Error)]
pub enum DispatchError {
    #[error("no eligible node for the requested constraints")]
    NoEligibleNode,
    #[error("dispatch timed out waiting for a result")]
    Timeout,
    #[error("dispatch cancelled: {0}")]
    Cancelled(String),
    /// The caller passed a `target_node_id` that isn't in the registry.
    #[error("target node not found: {0}")]
    NodeNotFound(String),
    /// The caller passed a `target_node_id` that exists but isn't idle.
    /// `reason` is a short label like "busy" or "draining" so callers
    /// can decide whether to retry or pick another node.
    #[error("target node {node_id} is not idle: {reason}")]
    NodeNotIdle { node_id: String, reason: String },
    /// The caller provided neither `target_node_id` nor `constraints`.
    /// Exactly one of the two must be set — this is the refactor that
    /// pushes routing decisions onto the (LLM) caller.
    #[error("dispatch requires exactly one of `target_node_id` or `constraints`")]
    NoTargetOrConstraints,
}
