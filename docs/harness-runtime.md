# Harness runtime contracts

Dyson's agent loop exposes a stable execution protocol for orchestration,
recovery, evaluation, and observability. The protocol is intentionally
separate from UI events and provider-specific stream formats.

The protocol, execution declarations, scheduler, replay logic, and
deterministic grader are owned by the standalone `dyson-harness` crate.
Provider/controller code depends on that crate through the compatibility
façade at `dyson::agent`; the harness never depends back on the application.

## Run outcomes

`Agent::run_detailed` returns a `RunOutcome` with a unique run id, a typed
terminal status, final text, and per-run token/call usage. Existing callers can
continue using `Agent::run`, which returns the final text.

Terminal status is one of `completed`, `cancelled`, `budget_exceeded`,
`iteration_limit`, `partial`, or `failed`. `paused` and `waiting_for_input`
are nonterminal outcomes: the caller can release its worker and later resume
the same run. Callers must not infer success from a non-empty string.

## Durable continuation and human input

With a `ChatHistory` backend attached, each run writes an atomic, versioned
`harness/continuation.json`. It contains its run identity, model iteration,
selected tool calls (including their arguments), transcript, token usage,
per-tool limits, repeated-failure counters, continuation text, and configuration
fingerprints. The pure `RunCursor::reduce` state machine owns transitions;
the agent executes effects between persisted boundaries. The existing task
ledger remains authoritative for shared task evidence, receipts, and budgets.

Pause is cooperative: it takes effect before the next model request or tool
phase. A tool already executing is allowed to finish. Before any tool starts,
its invocation is saved; its complete result is saved before delivery to the
transcript/controller. On resume, committed results are reused. A journaled
start with no observed outcome requires operator reconciliation; it is never
silently re-executed. If the tool finished but its result receipt was lost,
the agent receives an explicit recovery result directing it to task evidence.
Direct skill commands use the same invocation receipts. Resuming an interrupted
command recovers its result without starting a model turn.

The HTTP API exposes these authenticated, CSRF-protected controls:

| Method and path | Body / behavior |
|---|---|
| `GET /api/conversations/:id/run` | Current saved run, phase, pending tool names, and human question |
| `POST /api/conversations/:id/pause` | `{"run_id":"run-…"}`; request pause at the next boundary |
| `POST /api/conversations/:id/resume` | `{"run_id":"run-…"}`; resume the same saved run |
| `POST /api/conversations/:id/signal` | Run ID plus `answer` below; persist the answer and resume |
| `POST /api/conversations/:id/cancel` | Cancel active or suspended work |

For example, answering the default text form:

```json
{
  "run_id": "run-…",
  "answer": {
    "request_id": "tool-call-id",
    "action": "accept",
    "content": { "answer": "main" }
  }
}
```

`request_human_input` takes `question` and an optional object `schema` with
primitive fields, enums, required fields, string lengths, and numeric bounds.
It saves the request and yields without holding a worker or a five-minute
timer. The existing elicitation UI lists these requests alongside MCP forms,
and answering a durable form uses the same signal path. Answers are validated
on the server and persisted before acknowledgement. Duplicate or stale answers
cannot launch another run. Declining/cancelling a question withholds the rest
of its selected tool batch, allowing the model to reconsider the plan.

Controllers using `run_detailed` with attached history also accept `/resume`,
`/answer {"answer":"main"}`, and `/decline`. Resume does not add another user
turn, reset usage/iteration limits, or allocate a new run ID. Existing elapsed
task budgets continue to apply, including time spent paused. Changed model,
prompt, tool schemas, or working directory must be restored before resumption.
No boot-time auto-execution is performed: a caller supplies a resume/signal.
An acknowledged answer survives a crash before dispatch and can be continued
with `/resume` without answering again.

These boundaries cover Dyson's native executor. CLI providers own their
inner execution and do not receive the native human-input tool. Arbitrary MCP
server-originated requests still depend on that server's live protocol session;
the durable tool is a separate option for restart-safe human interaction.
Subagents return detailed run outcomes in metadata, and the parent receives
the status, warnings, and task evidence contract in its tool-result context.
Incomplete children are never labelled successful solely because they returned
text. Raw report text remains available to staged report parsers.

## Durable execution journal

When a `ChatHistory` backend is attached, the agent appends versioned `RunEvent`
records to `run-events.jsonl`. The disk backend writes each record before a
side-effecting tool begins and syncs it to stable storage. A torn final record
is discarded and repaired on the next append; malformed committed records fail
replay loudly.

The important tool lifecycle is:

1. `tool_requested` records the original model request and a hash of its input.
2. `tool_authorized` records the effective tool after policy and its execution
   contract.
3. `tool_started` is synced immediately before dispatch.
4. `tool_finished` records the observed result and duration.
5. `tool_outcome_unknown` records an explicit timeout or indeterminate result.

On restart, `Agent::unresolved_tool_outcomes` finds starts without terminal
events. These calls are surfaced for reconciliation and are never silently
retried. `protocol::evaluate_run` grades the same canonical trajectory in CI or
against a live model matrix.

## Scheduling and idempotency

Every tool declares a `ToolExecutionPlan`: resource read/write claims,
idempotency class, and hard timeout. Reads of the same resource may run in
parallel; any overlapping write is serialized. Tools without a declaration are
assigned a global exclusive claim, preserving safety at the cost of
parallelism.

Core file tools use lexically normalized file resource keys. New tools should
declare the narrowest stable resource identity they can defend. `unsafe` tools
must treat a missing terminal journal event as an unknown outcome, not proof
that the side effect did not happen.

## Validation and stream completion

Tool inputs are checked centrally against the portable JSON Schema subset
before execution. A provider stream is successful only after a terminal
`message_complete` event; EOF is an error unless complete tool calls have
already been received, in which case Dyson preserves those calls and refuses a
blind retry.

Context size uses a conservative tokenizer-independent estimator based on the
maximum of word count and Unicode character count divided by four. Provider
reported usage remains authoritative for budgets and accounting. Compaction
never resets lifetime token usage.

## Evaluation gate

A production model evaluation should run the same task corpus across the model
and provider matrix, persist the journal, then combine:

- deterministic protocol grading (`evaluate_run`);
- task-specific assertions on workspace state and final answers;
- latency, token, retry, tool-error, and unknown-outcome thresholds;
- fault cases for truncated streams, cancellation, timeout, torn journals,
  malformed tool input, and process termination after `tool_started`.

Mocked tests remain the fast CI layer. They are not a substitute for the live
matrix, and live runs should publish their corpus version, model identifiers,
grader version, raw journal, and aggregate confidence intervals.

## Enforced runtime behavior (September 2026)

- Global exclusive execution claims conflict with every resource. File plans use
  the schema's `file_path` field, and pre-tool rewrites retain the original plan
  for footprint validation.
- HTTP turns attach both transcript persistence and the durable execution
  journal. Failed start/authorization writes prevent dispatch. Finished tool
  results enter history before output delivery; delivery failures produce run
  warnings without discarding subsequent batch results.
- Unknown outcomes remain unresolved until an operator records a resolution.
  Reads can investigate them; further mutations are withheld. For a loaded,
  idle conversation, authenticated operators can inspect
  `GET /api/conversations/:id/recovery` and resolve with
  `POST /api/conversations/:id/recovery`, supplying `run_id`, `tool_use_id`, and
  a nonempty `resolution` describing the verified evidence. Resolution is never
  an automatically exposed model tool.
- HTTP emits `run_outcome` before `done`. The UI surfaces non-completed status;
  terminal, Telegram, and background controllers also consume typed outcomes.
  Multimodal calls have the same detailed outcome API. Failed detailed calls
  retain their run id, known usage, and warnings.
- Every observed main-loop response, empty retry, compaction, and final summary
  contributes to usage. Failed streams account for visible generation with a
  local estimate when authoritative provider usage is unavailable. Request caps
  respect remaining output budget; up to 10% (at most 1,024 tokens) is reserved
  for a tool-free final summary. Provider-internal retries without surfaced
  usage are conservatively reserved in the shared task budget; Swarm remains
  authoritative for billing.
- Compaction retains bounded tool evidence from both the beginning and end of
  each result, including error flags. It refuses truncated summaries and keeps
  original history on failure. Full pre-compaction transcripts are archived
  when a history backend is available.
- Tool limits span a complete user turn. Three identical failed invocations
  trigger a changed-approach instruction and block identical retries. Five
  unchanged successful observations trigger a progress warning without
  prohibiting legitimate polling.
- Background learning tools use the normal sandbox, schema validation, journal,
  timeout, and execution hooks available to their restricted executor. Before
  background changes, workspace pre-images are saved under
  `improvement/before-*.json` for inspection and restoration. Synthesis uses the
  same restricted path as maintenance.

## Regression corpus

The executable deterministic corpus is in `agent/audit_tests.rs`, the harness
scheduler/protocol regression modules, the HTTP controller integration test,
and `run-outcome.test.js`. It asserts actual filesystem state, captured model
context, terminal outcomes, usage, journal replay, and browser event delivery.
It includes journal failure, interrupted mutation, explicit reconciliation,
output disconnection, truncated generation, compaction evidence, repeated
failures, unchanged reads, and exhausted budgets. These checks complement the
existing end-to-end agent corpus; they do not claim a live model quality score.

## Durable task control

Every agent exposes `task_control`. Before an action task, `plan` records an
objective, steps and acceptance criteria (`id`, `description`, `tool`, and a
nonempty expected output substring in `contains`). `update` records remaining
steps, completed steps and blockers. `status` returns the ledger and shared
budget. Replacing an unfinished plan requires `replace: true` and should only
follow a user change of objective.

`verify` binds a criterion to an `evidence_id` produced by an actual successful
tool call. The tool name and output must match the criterion; changed resources
or subsequent mutations invalidate verification. `RunOutcome.task.completion`
is `verified` when all declared checks pass without blockers, `unverified` for
unfinished action tasks, or `answered` for a response without an action contract.
Unverified action tasks end with `partial`, even when the model says “done”.
Verification establishes the declared checks, not general task correctness.
Model evaluations and their release policy remain in Swarm.

The disk backend atomically stores versioned task state, pending mutation
claims, receipts and budgets under `<chat>/harness/task.json`. Full redacted
tool results live in separate content-hashed evidence records. They survive
compaction and process restarts; `task_control` with `action: "evidence"`,
`evidence_id`, `offset` and `limit` retrieves character pages (at most 16,000
characters). Corrupt or unsupported checkpoints fail recovery explicitly.
After reconstruction, criteria need fresh verification; objectives, steps,
blockers, evidence and accounting remain available.

## Coordination and recoverable effects

Process-wide leases serialize overlapping writes across independent
conversations. File claims resolve existing symlink ancestors. Child agents
inherit their parent's lease ancestry and share its ledger and budget, avoiding
self-deadlock while serializing competing children. File observations are
checked under the lease before a write; a stale observation requires rereading
the resource. This coordinates one runtime, not external writers or a fleetwide
transaction system.

An interrupted mutation quarantines its claims until explicit reconciliation.
Pending claims are persisted and checked against unresolved journals from other
stored conversations after restart. Read-only investigation remains allowed.
External writes are detected at file observation/verification boundaries where
fingerprints are available; resource declarations must accurately describe a
tool's footprint.

Tools declaring `Idempotency::Keyed` must implement `idempotency_key` with a
stable provider operation key and use `ToolContext.idempotency_key` when making
the request. Durable receipts deduplicate the same key and input; changed input
under the same key is refused. `lookup_result` may return a provider-confirmed
result to reconcile an indeterminate call. Without confirmation the runtime
withholds reexecution. This is an opt-in provider contract, not an exactly-once
guarantee for arbitrary tools. Receipt replay preserves text and error status;
rich attachments require a tool-specific durable result reference.

## Progress and shared budgets

Eight calls without a new successful output request a strategy change; sixteen
pause the run with `partial`. Merely changing command arguments does not count
as progress. Criterion verification resets the stall counter. A new root user
turn can resume the task without erasing its durable work or consumed budget.

`task_budget` in agent configuration optionally caps input tokens, output tokens,
cost in micro-USD and elapsed milliseconds. Reservations are shared by parents,
children, background reflection, compaction, summaries and provider retries.
Reservations are persisted before dispatch. Successful calls settle against
reported usage; uncertain failures retain their reserved charge. Output caps
are reduced to available capacity. Elapsed time includes downtime, and restored
limits cannot relax tighter operator configuration.

Cost caps require explicit per-model prices; an unknown model price refuses the
request when a cost cap is set. Prices are configuration estimates and do not
replace Swarm billing. Input estimates and provider reporting can differ, so
actual reported usage is charged even if it exceeds a reservation; further
calls are then withheld. Limits default to unset. The model cannot raise them.
