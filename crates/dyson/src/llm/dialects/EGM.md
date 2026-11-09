# LLM Dialects (`llm/dialects`)

## Overview
Model-family adaptations for tool calling. Some models emit tool calls as plain text instead of using the structured `tools` API; a dialect advertises tools in the system prompt and extracts calls from text, and `TextToolExtractorStream` converts them into standard `StreamEvent`s so the agent loop is unaffected. The DeepSeek dialect handles thinking-mode `reasoning` parsing and the outbound `reasoning_content` echo.

## Interface
- `TextToolHandler` trait: `format_tools_for_prompt(&[ToolDefinition]) -> String`, `extract_tool_calls(&str) -> Option<(String, Vec<ExtractedToolCall>)>` [mod.rs:TextToolHandler]
- `ExtractedToolCall` [mod.rs:ExtractedToolCall]
- `text_tool_handler_for_model(model) -> Option<Box<dyn TextToolHandler>>` - Gemma, then Qwen, else `None` [mod.rs:text_tool_handler_for_model]
- `TextToolExtractorStream` (crate) - buffers text and emits `ToolUseComplete` events at stream end [mod.rs:TextToolExtractorStream]
- `write_tool_param_table` (crate) - shared parameter table renderer [mod.rs:write_tool_param_table]
- `gemma::{is_gemma_model, GemmaToolHandler}` [gemma.rs:GemmaToolHandler]
- `qwen::{is_qwen_model, QwenToolHandler}` [qwen.rs:QwenToolHandler]
- `deepseek::{is_deepseek_model, DeepSeekJsonParser, inject_reasoning_content}` [deepseek.rs:DeepSeekJsonParser, deepseek.rs:inject_reasoning_content]

## Invariants
- New text dialects must implement `TextToolHandler` and be registered in `text_tool_handler_for_model` [mod.rs:text_tool_handler_for_model]
- `DeepSeekJsonParser` (an `SseJsonParser`) also captures OpenRouter's `delta.reasoning` when `reasoning_content` is absent [deepseek.rs:DeepSeekJsonParser]

## Side Effects
- None (pure prompt/stream transformations) [mod.rs:TextToolExtractorStream]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
