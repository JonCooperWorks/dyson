# Bug report: Hunt per-child wall-clock timeout degrades every class on large repos

**Date:** 2026-06-09
**Severity:** High (silent quality degradation — run "succeeds" but coverage is near-zero)
**Component:** `crates/dyson/src/skill/subagent/security_engineer/stages.rs` — `HUNT_CHILD_TIMEOUT`
**Found on:** sparky (`dynamic-eel-946-4b26de`), deepseek-v4-flash, security_engineer harness on `vllm-project/vllm` (chat `c-0023`, run `sec-1780985376-2`).

## Symptom

The harness ran end-to-end and returned success (`is_error=false`, all 8 stages), but the
report was nearly empty: **7 findings (1 critical / 1 high / 3 medium / 2 low) and 49 coverage
gaps**. The baked event stream shows **all 24 taxonomy class specialists** marked:

```
security_engineer: hunt: class <X> degraded (specialist did not complete)
```

Only the two stack specialists (`lang/rust`, `framework/aiohttp`) completed and contributed
findings. Total runtime ~46 min (`security_engineer: ok (2767180ms)`), **0 LLM errors**.

For contrast, the same harness/model on the tiny intentionally-vulnerable Flask app produced
60 findings across all 24 classes with **0** degraded specialists.

## Root cause

`HUNT_CHILD_TIMEOUT` was `Duration::from_secs(420)` (7 min) — a per-specialist wall-clock
budget enforced in `dispatch_hunts` via `tokio::time::timeout`. On a small target a class
specialist finishes well inside 7 min, so the bound never trips. On a large repo (vLLM) a
class specialist legitimately needs much longer — it walks a big tree with repeated
`ast_query` / `taint_trace` / `read_file` calls across up to `HUNT_MAX_ITERATIONS` (28)
agent-loop turns. Every class specialist blew the 7-min budget and was folded by
`fold_hunt_degraded` into a coverage gap.

The fold-as-degraded behavior is correct and is what kept the run from deadlocking/crashing
(the resilience fix working as designed). The defect is purely that the **timeout value was
tighter than the legitimate worst-case child runtime**, so the backstop fired on healthy work.

Crucially, the wall-clock timeout is *not* the primary anti-hang mechanism — that is the
transport read timeout (`http::READ_TIMEOUT`, 120s), which makes any stalled LLM stream error
within ~2 min so the child's agent loop retries or returns instead of blocking forever.
Combined with the per-child iteration cap, a healthy specialist always terminates on its own.
The wall-clock budget only needs to catch a child wedged in a way neither covers (e.g. a hung
non-HTTP tool) — so it should be sized generously, never tight enough to cut real work.

## Fix

Size `HUNT_CHILD_TIMEOUT` from the iteration cap rather than a flat too-small constant:

```rust
const HUNT_CHILD_TIMEOUT: Duration =
    Duration::from_secs(HUNT_MAX_ITERATIONS as u64 * 120);   // 28 * 120 = 3360s (56 min)
```

Rationale: each agent-loop iteration's LLM call is bounded by the read timeout (~120s worst
case before it errors), and there are at most `HUNT_MAX_ITERATIONS` of them, so a full-depth
specialist completes within ~`iterations * read_timeout`. Budgeting 120s/iteration makes a
false cut effectively impossible while still bounding a truly-wedged child to under an hour.
Small targets are unaffected (their specialists finish in 1–7 min, far under the new bound).

## Regression test

`hunt_child_timeout_is_generous_relative_to_iteration_cap` (in `stages.rs`) asserts
`HUNT_CHILD_TIMEOUT >= HUNT_MAX_ITERATIONS * 90s`, encoding the invariant that the wall-clock
backstop must never be tighter than the legitimate worst-case child runtime. Setting it back
to 420s (the buggy value) fails the test.

## Cost asymmetry (why bias generous)

Cutting a legitimately-progressing specialist is far more costly than letting a rare wedged
child run longer: a premature cut silently degrades an entire vulnerability class to a coverage
gap, and the run still reports "success" — so the damage is invisible unless you read the
coverage gaps. A slightly-too-high backstop only wastes a few minutes on the rare genuinely
wedged child.

## Not done (per instruction)

The vLLM harness was **not** re-run — per the operator's explicit instruction to fix the issue
without re-running the expensive vLLM review. The fix is landed and deployed for future runs.
