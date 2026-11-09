# dyson-harness src

## Overview
This crate holds the agent-harness contracts and has no dependency on providers or controllers ([lib.rs:module doc]). It defines the tool-call value type, tool scheduling metadata, a dependency-aware scheduler that groups tool calls into parallel or sequential phases, a pure state machine for run continuations, and the versioned run-event protocol. Persistence, replay, evaluations and observability share that protocol ([protocol.rs:module doc]).

## Interface
- Re-exports ([lib.rs:pub use]):
  - `ToolCall`, `Idempotency`, `ResourceAccess`, `ResourceClaim` and `ToolExecutionPlan`.
  - `RUN_EVENT_SCHEMA_VERSION`, `RunEvaluation`, `RunEvent`, `RunEventKind`, `RunId`, `RunOutcome`, `RunStatus`, `RunUsage` and `UnresolvedToolOutcome`.
  - `evaluate_run` and `unresolved_tool_outcomes`.
  - `DependencyAnalyzer` and `ExecutionPhase`.
- `ToolCall { id, name, input }` with `ToolCall::new(name, input)` ([call.rs:ToolCall]).
- `ToolExecutionPlan { resources, idempotency, timeout_ms }` with the constructors `exclusive()`, `read(key)` and `write(key)`. `ResourceAccess::{Read, Write}` and `Idempotency::{Safe, Keyed, Unsafe}` ([contracts.rs]).
- `DependencyAnalyzer::analyze_plans(&[ToolExecutionPlan])` (production path) and `DependencyAnalyzer::analyze(&[&ToolCall])`. Both return `Vec<ExecutionPhase::{Parallel(Vec<usize>), Sequential(Vec<usize>)}>` ([scheduler.rs:DependencyAnalyzer]).
- `continuation` module ([continuation.rs]):
  - `RunCursor { run_id, state, iteration, pending }` with `new` and `reduce(Transition) -> Result<RunCursor, &'static str>`.
  - `RunState::{Running, Paused, WaitingForInput, Finished}`.
  - `Transition::{Selected, Observed, Pause, Wait, Resume, Answer, Finish}`.
  - `HumanInputRequest`, `HumanAnswer` and `HumanAction::{Accept, Decline, Cancel}`.
- `protocol` module ([protocol.rs]):
  - `RunId(String)` with `RunId::new()`.
  - `RunStatus` with eight variants.
  - `RunEventKind` with 14 tagged variants, `RunStarted` through `RunFinished`.
  - `RunEvent { schema_version, sequence, unix_ms, run_id, turn, kind }` with `RunEvent::new`.
  - `ToolOutcomeIndex::{apply, unresolved}`.
  - `RunOutcome`, `RunUsage` and `RunEvaluation`.

## Invariants
- `RUN_EVENT_SCHEMA_VERSION = 1`. `RunEvent::new` stamps it ([protocol.rs:RUN_EVENT_SCHEMA_VERSION]).
- `evaluate_run` collects a failure for each of these ([protocol.rs:evaluate_run]):
  - Not exactly one `run_started`.
  - A non-increasing `sequence`.
  - An event after `run_finished`, or other than exactly one `run_finished`.
  - A wrong schema version.
  - A duplicate request or authorization, or an authorization without a request.
  - A tool started without a matching authorization or started twice, a tool finished without starting, or multiple terminal events.
  - Any `ToolOutcomeUnknown`.
  - Overlapping or unfinished model attempts.
  - A final status other than `Completed`.
  - Any unresolved tools.
- Crash-recovery rule: a `ToolStarted` with no later `ToolFinished` or `ToolReconciled` stays an `UnresolvedToolOutcome`. The protocol doc says these must be surfaced and never retried silently. Results are sorted by `(run_id, tool_use_id)` ([protocol.rs:UnresolvedToolOutcome], [protocol.rs:ToolOutcomeIndex::unresolved]).
- `RunCursor::reduce` is pure. It rejects invalid transitions without mutating input, and returns `"tool call IDs must be nonempty and unique"` for empty, repeated or previously seen IDs. `Selected` is allowed only while `Running` with no pending calls, and it increments `iteration`. `Wait` requires a matching pending call ([continuation.rs:RunCursor::reduce]).
- Scheduler dependencies ([scheduler.rs:has_dependency]):
  - A dependency exists on write-after-read, read-after-write and write-after-write.
  - The `"global:tool-execution"` claim (`ToolExecutionPlan::exclusive`) is a barrier against every other call.
- Scheduler phases are layers by depth. A layer becomes `Sequential` if any of its calls conflict, or if it is a single call with dependencies ([scheduler.rs:phases_for_accesses]).
- `analyze` call classification ([scheduler.rs:extract_resources]):
  - Every `bash` call claims a shared `Shell` resource.
  - A bash command counts as a read only when its executable is one of `cat`, `echo`, `grep`, `head`, `ls`, `printf`, `pwd`, `rg`, `stat`, `tail` or `wc`, and the command contains no `>`, `|`, `;`, `&&`, `||`, `$(` or backtick. Everything else is a write ([scheduler.rs:classify_shell_command]).
  - Git commands come from `GIT_WRITE_COMMANDS` and `GIT_READ_COMMANDS`. An unknown `git ` command is a write ([scheduler.rs:classify_git_command]).
- Default timeouts: `exclusive()` is 300_000 ms with `Unsafe`. `read()` is 120_000 ms with `Safe`. `write()` is 120_000 ms with `Unsafe` ([contracts.rs:ToolExecutionPlan]).

## Side Effects
- Randomness: `RunId::new()` uses `rand::random::<u128>()` and formats `run-{:032x}` ([protocol.rs:new_id]).
- Global state: a process-wide `AtomicU64` counter for `ToolCall::new` ids in the form `call_{name}_{n}` ([call.rs:ToolCall::new]).
- Clock read: `RunEvent::new` reads `SystemTime::now()` ([protocol.rs:RunEvent::new]).
- No I/O.

## Verification
- Run: `cargo test -p dyson-harness` (from `dyson/`)
