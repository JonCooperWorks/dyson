# Task Budget and Leases (`agent/task`)

## Overview
Shared accounting and concurrency primitives for task execution. `budget.rs` keeps integer token/cost/elapsed budgets with reservations shared by every child of a task. `leases.rs` maintains process-wide resource leases so independent conversations and nested tool executions do not perform conflicting writes, with quarantine for unknown outcomes and content fingerprints for change detection.

## Interface
- `now_ms()` - wall-clock milliseconds since epoch, saturating [budget.rs:now_ms]
- `Limits { max_input_tokens, max_output_tokens, max_cost_microusd, max_elapsed_ms, prices }` and `Limits::restrict(&configured)` [budget.rs:Limits]
- `Price { input_microusd_per_million, output_microusd_per_million }` [budget.rs:Price]
- `BudgetState` - running totals and reservation count [budget.rs:BudgetState]
- `Reservation` - created by `reserve(...)`, finalised by `settle(input, output)`, released on `Drop` [budget.rs:Reservation]
- `charge_uncertain_retry(model, input, output)` [budget.rs:charge_uncertain_retry]
- `Lease { id }` with `arm`, `complete`, `quarantine`; `acquire(plan, ctx) -> Result<Lease>` [leases.rs:Lease, leases.rs:acquire]
- `reconcile(key)` - clears a quarantined lease [leases.rs:reconcile]
- `note_mutation()`, `fingerprints(claims)`, `unchanged(sources)` - mutation epoch and content stamps [leases.rs:note_mutation, leases.rs:fingerprints, leases.rs:unchanged]

## Invariants
- `Limits::restrict` only tightens: each bound becomes the minimum of existing and configured values [budget.rs:Limits::restrict]
- Prices are never inferred from a model name; unknown models use `Price::default()` [budget.rs:Limits, budget.rs:Reservation::reserve]
- Reservation fails with `task input-token budget exhausted`, `task cost budget exhausted`, or `task output/cost budget exhausted`; all arithmetic saturates [budget.rs:Reservation::reserve]
- `settle` replaces the reserved estimate with actual usage [budget.rs:Reservation::settle]
- Two claims conflict when keys overlap and at least one is `ResourceAccess::Write`; `global:tool-execution` overlaps everything and `file:` keys overlap on path-prefix boundaries [leases.rs:overlap, leases.rs:conflict]
- `file:` claim keys are canonicalised (existing ancestor resolved, suffix re-appended) before comparison [leases.rs:canonical_claim]
- Dropping a lease that is still armed quarantines its resources until `reconcile` is called [leases.rs:Lease::drop]

## Side Effects
- Process-global lease table `HELD`, id counter `NEXT`, and `Notify` waiters [leases.rs:HELD]
- Global `MUTATION_EPOCH` counter bumped by `note_mutation` [leases.rs:note_mutation]
- Reads file metadata and contents (SHA-256) to fingerprint claimed paths [leases.rs:resource_stamp]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
