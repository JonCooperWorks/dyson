//! Agent-harness contracts with no dependency on providers or controllers.

mod call;
mod contracts;
pub mod protocol;
pub mod scheduler;

pub use call::ToolCall;
pub use contracts::{Idempotency, ResourceAccess, ResourceClaim, ToolExecutionPlan};
pub use protocol::{
    RUN_EVENT_SCHEMA_VERSION, RunEvaluation, RunEvent, RunEventKind, RunId, RunOutcome, RunStatus,
    RunUsage, UnresolvedToolOutcome, evaluate_run, unresolved_tool_outcomes,
};
pub use scheduler::{DependencyAnalyzer, ExecutionPhase};
