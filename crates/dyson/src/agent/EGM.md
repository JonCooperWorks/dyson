# Agent Loop (`agent`)

## Overview
The streaming tool-use loop at the heart of Dyson. `Agent` sends the conversation to the LLM, processes the stream into an assistant message and tool calls, rate-limits the calls, groups them into dependency phases (parallel or sequential), executes each through the sandbox (`check` -> `run` -> `after`), formats results, and repeats until the model stops calling tools or `max_iterations` is reached. Supporting modules handle compaction, durable continuations for suspended runs, background "dreams" (memory curation and self-improvement), priority rate limiting, result formatting/sanitisation, and persistence hooks. Task budgets and resource leases live in `task/`.

## Interface
- `Agent` - `builder(...)`, `new(...)`, `run`, `run_with_blocks`, `run_with_attachments`, `run_with_attachments_detailed`, `run_detailed`, `clear`, `messages`, `set_messages`, `append_direct_turn`, `swap_client`, `register_tool`, `has_tool`, `tool_names`, `system_prompt`, `sandbox`, `set_tool_hooks`, `set_working_dir`, `set_cancellation_token`, `set_activity_handle`, `set_subagent_events`, `set_artefact_reader`, `set_feedback_store`, `record_feedback`, `remove_feedback`, `set_attribution` [mod.rs:Agent]
- Builder: `skills`, `settings`, `workspace`, `transcriber`, `advisor`, `build` [mod.rs:Agent::builder]
- `quick_response(...)` [mod.rs:quick_response]; `execute_tool_direct(...)` [execution.rs:execute_tool_direct]
- `process_stream(...)`, `ToolCall` (from `dyson_harness`) [stream_handler.rs:process_stream]
- Dreams: `Dream` trait, `DreamRunner`, `DreamHandle`, `DreamTrigger`, `DreamEvent`, `DreamContext`, `DreamOutcome` [dream.rs:Dream, dream.rs:DreamRunner]; `LearningSynthesisDream`, `MemoryMaintenanceDream`, `SelfImprovementDream` [reflection.rs:LearningSynthesisDream, reflection.rs:MemoryMaintenanceDream, reflection.rs:SelfImprovementDream]
- Continuations: `RunCheckpoint` (`load`, `cancel`, `pending_input`, `accept_answer`, `validate_resume`), `resume_command`, `RECORD = "continuation"`, re-exports `HumanAction`, `HumanAnswer`, `RunCursor`, `RunState`, `Transition` [continuation.rs:RunCheckpoint]
- Rate limiting: `Priority { Scheduled, Background, ... }`, `RateLimited<T>`, `RateLimitGuard`, `RateLimitedHandle<T>` [rate_limiter.rs:Priority, rate_limiter.rs:RateLimitedHandle]
- `ResultFormatter`, `FormattedResult`, `sanitize_tool_output` [result_formatter.rs:ResultFormatter, result_formatter.rs:sanitize_tool_output]
- `ToolLimiter` [tool_limiter.rs:ToolLimiter], `TokenBudget` [token_budget.rs:TokenBudget], `SlidingWindow` [sliding_window.rs:SlidingWindow], `SilentOutput` [silent_output.rs:SilentOutput]
- `task` (pub): `PROMPT`, `digest`, `canonical_json`, `acquire`, `reconcile_lease` [task.rs:digest] - see [task](./task/EGM.md)
- Facades: `protocol` re-exports `dyson_harness::protocol::*` [protocol.rs]; `dependency_analyzer` re-exports `dyson_harness::scheduler::*` [dependency_analyzer.rs]

## Invariants
- Every tool call passes through the sandbox before and after execution [mod.rs:Agent]
- The tool registry is immutable after construction [state.rs:ToolRegistry]
- Per-turn tool limit defaults to 50 calls with a 1 s cooldown [tool_limiter.rs:DEFAULT_PER_TURN_LIMIT, tool_limiter.rs:DEFAULT_COOLDOWN]
- Priority capacity shares: `Scheduled` up to 1/3, `Background` up to 2/3, interactive full [rate_limiter.rs:Priority]
- At most `MAX_CONCURRENT_DREAMS = 4` dreams run concurrently [dream.rs:MAX_CONCURRENT_DREAMS]
- Tool output over `TRUNCATION_THRESHOLD = 30_000` is truncated; chat-template tokens (e.g. `<|im_start|>`, `<|eot_id|>`) and `<system-reminder` markers are neutralised in tool output [result_formatter.rs:TRUNCATION_THRESHOLD, result_formatter.rs:sanitize_tool_output]
- `<think>` buffering is capped at 8 KB before flushing as thinking text [stream_handler.rs:MAX_PENDING]
- The continuation checkpoint is the authority for a suspended run; transcripts and SSE are projections of it [continuation.rs]
- Compaction prunes middle tool outputs, protects head/tail, summarises the middle via LLM, and repairs orphaned tool_use/tool_result pairs [compaction.rs]

## Side Effects
- LLM calls through the rate-limited client and tool execution (which may touch filesystem, network, processes) [mod.rs:Agent, execution.rs:execute_tool_direct]
- Persists transcripts via a persist hook and chat-history backend [persistence.rs:set_persist_hook, state.rs:HistoryBackend]
- Dreams spawn background tasks that write workspace files (`MEMORY.md`, `USER.md`, skills, `improvement/{epoch}.json`) [dream.rs:DreamRunner, reflection.rs:MemoryMaintenanceDream]
- Stores continuation checkpoints in chat history under record `continuation` [continuation.rs:RunCheckpoint]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
