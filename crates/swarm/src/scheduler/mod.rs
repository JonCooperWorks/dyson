//! The scheduler service.
//!
//! The scheduler is the long-running task lifecycle manager. It owns:
//!
//! - a SQLite-backed task store (so a hub restart doesn't lose history)
//! - the submit / poll / cancel state machine
//! - per-task progress and log capture
//! - cancellation orchestration with skill-side grace timers
//! - the "agent reachback" surface (`recent_results`) so an agent that
//!   exits during a long task can catch up when it boots back up
//!
//! It does **not** know about the wire transport. Today the hub wires
//! it directly into the existing `/swarm/*` HTTP endpoints; tomorrow it
//! will consume its inbox via [`dyson_mesh::MeshClient`] and the wiring
//! disappears.
//!
//! See `docs/swarm.md` for the full lifecycle.

pub mod store;
pub mod types;

pub use store::{TaskStore, TaskStoreError};
pub use types::{
    NotifyChannel, ProgressReport, RecentResult, SubmitRequest, TaskRow, TaskState,
    TerminalState, TerminalStatus,
};
