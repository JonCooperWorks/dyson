//! Task queue — placeholder for v2.
//!
//! In v1, the hub has no pending queue: if no node is idle when a
//! dispatch request arrives, `swarm_dispatch` returns an error
//! immediately.  This module exists so v2 has a natural home.

use thiserror::Error;

/// Errors produced by the dispatcher when it can't place a task.
///
/// The variants split into three retry-classes so callers can decide
/// without parsing a message string:
///
///   * **terminal, caller error** — `InvalidArgs`, `NoTargetOrConstraints`.
///     Retrying with the same input will fail the same way.
///   * **terminal, environment** — `NoEligibleNode`, `NodeNotFound`,
///     `NodeNotIdle`.  Caller may retry after re-inspecting the cluster.
///   * **transient** — `Transient`, `Timeout`.  A retry of the identical
///     request may succeed; exponential backoff is appropriate.
#[derive(Debug, Error)]
pub enum DispatchError {
    #[error("no eligible node for the requested constraints")]
    NoEligibleNode,
    #[error("dispatch timed out waiting for a result")]
    Timeout,
    /// Transient failure: serialization error, result channel closed,
    /// SSE stream gone between node selection and push, persistence
    /// failure.  The input was valid; the cluster state was unstable.
    #[error("dispatch transient failure: {0}")]
    Transient(String),
    /// Caller-side validation failure.  Retry will not help unless the
    /// caller changes the arguments.
    #[error("invalid dispatch arguments: {0}")]
    InvalidArgs(String),
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

impl DispatchError {
    /// True if a retry of the identical request may succeed.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_) | Self::Timeout)
    }
}
