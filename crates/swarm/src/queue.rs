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
}
