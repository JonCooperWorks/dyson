# dyson-core src

## Overview
This crate holds the stable, provider-neutral domain contracts shared by every Dyson subsystem: the conversation message model, artefacts, and the unified error type. Per its module doc it deliberately does not depend on the agent, controllers, tools, persistence or provider crates ([lib.rs:module doc]).

## Interface
- Re-exports ([lib.rs:pub use]):
  - `DysonError` and `Result`.
  - `Artefact`, `ArtefactKind`, `ContentBlock`, `Message`, `MessageCostMetadata` and `Role`.
  - `estimate_json_tokens` and `estimate_text_tokens`.
- `DysonError` variants ([error.rs:DysonError]):
  - `Llm` and `LlmRateLimit { retry_after }`.
  - `LlmOverloaded { retry_after }`.
  - `Tool { tool, message }`, `Mcp { server, message }` and `OAuth { server, message }`.
  - `Config`.
  - `Io`, `Http` and `Json`, each with a `#[from]` conversion.
  - `Cancelled` and `RateLimit { limit, window_secs }`.
- Constructors `DysonError::tool`, `::mcp` and `::oauth`, plus `sanitized_message()` ([error.rs:impl DysonError]).
- `type Result<T> = std::result::Result<T, DysonError>` ([error.rs:Result]).
- `LlmRecovery::{GiveUp, RetryWithoutTools, RetryWithoutImages}` and `LlmErrorKind::{NoToolUse, NoVision, Other}`, both public in `error` but not re-exported ([error.rs:LlmRecovery], [error.rs:LlmErrorKind]).
- `classify_llm_error(&str) -> LlmErrorKind` ([error.rs:classify_llm_error]).
- `Role::{User, Assistant}` ([message.rs:Role]).
- `ContentBlock::{Text, ToolUse, ToolResult, Thinking, Image, Document, Artefact}` ([message.rs:ContentBlock]).
- `ArtefactKind::{SecurityReview, EvalReport, Image, Other}` ([message.rs:ArtefactKind]).
- `Artefact { id, kind, title, content, mime_type, metadata }` with `markdown()` and `with_metadata()` ([message.rs:Artefact]).
- `Message { role, content, cost, context_summary }` with the constructors `user`, `assistant`, `user_multimodal`, `context_summary` and `tool_result`, plus `last_text` and `estimate_tokens` ([message.rs:Message]).
- `MessageCostMetadata`: all fields optional (`swarm_llm_audit_id`, `display_cost_usd`, `cost_source`, `provider`, `model`, token counts, `key_source`) ([message.rs:MessageCostMetadata]).

## Invariants
- `sanitized_message()` passes application-level variants through unchanged: Llm*, Tool, Mcp, OAuth, Config, RateLimit and Cancelled. It collapses `Io`, `Http` and `Json` to the generic strings `"internal IO error"`, `"upstream HTTP error"` and `"internal JSON error"` so paths and upstream URLs are not sent to remote clients ([error.rs:sanitized_message]).
- `classify_llm_error` returns `NoToolUse` when the error contains "tool use" or "tool_use". It returns `NoVision` for specific image or vision substrings. Everything else is `Other` ([error.rs:classify_llm_error]).
- Serde wire format:
  - `ContentBlock` is tagged as `{"type": <snake_case>}` and `Role` is lowercase ([message.rs:ContentBlock], [message.rs:Role]).
  - `Message.cost` is skipped when `None`, and `context_summary` is skipped when false ([message.rs:Message]).
- `Artefact::markdown` sets `mime_type = "text/markdown"` and an empty `id` ([message.rs:Artefact::markdown]).
- `Message::context_summary` produces a user message prefixed with `[Context Summary]` and sets `context_summary = true` ([message.rs:Message::context_summary]).
- Token estimates:
  - Text: `max(word count, ceil(chars / 4), 1)` ([message.rs:estimate_text_tokens]).
  - Images: `max(decoded_bytes / 750, 100)` ([message.rs:ContentBlock::estimate_tokens]).
  - Artefact blocks: 0 ([message.rs:ContentBlock::estimate_tokens]).
  - Messages: add 4 tokens of framing ([message.rs:Message::estimate_tokens]).

## Side Effects
- None. These are pure data types and functions with no I/O or global state.

## Verification
- Run: `cargo test -p dyson-core` (from `dyson/`)
