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
`iteration_limit`, `partial`, or `failed`. Callers must not infer success from
a non-empty string.

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
