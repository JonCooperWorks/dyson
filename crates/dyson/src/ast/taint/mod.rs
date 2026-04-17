// Cross-file taint-reachability oracle for the security_engineer
// subagent.  Given a source `file:line` and sink `file:line`, returns
// ranked candidate call chains.  Name-based, lossy by design — the
// agent verifies each hop with `read_file` before filing.
//
// Data flow:
//   `build_index(language, working_dir)` → `SymbolIndex`  (cached on ToolContext)
//   `trace(index, config, …)`            → `TraceResult`  (ranked paths + flags)
//   `taint_trace` tool renders the result for the agent.

pub mod index;
pub mod trace;
pub mod types;

pub use index::{build_index, is_stale};
pub use trace::{TraceError, TraceOptions, TraceResult, trace};
pub use types::{Assignment, CallSite, FnDef, FnId, Hop, HopKind, SymbolIndex, TaintPath};
