# LLM Clients (`llm`)

## Overview
Provider-neutral LLM client layer. `LlmClient::stream` turns messages, a system prompt, tool definitions, and `CompletionConfig` into a stream of `StreamEvent`s. Implementations cover HTTP APIs (Anthropic, OpenAI, OpenAI-compatible such as OpenRouter/Ollama, Gemini) and CLI subprocess backends (Claude Code `claude`, OpenAI `codex`), which receive Dyson's tools through a loopback MCP server. Shared infrastructure handles SSE parsing with size guards, retries with backoff, concurrency limits, the provider registry, and pricing tables.

## Interface
- `LlmClient` trait - `stream(...)` [mod.rs:LlmClient]
- `CompletionConfig`, `ToolDefinition`, `ToolMode`, `StreamResponse` [mod.rs:CompletionConfig, mod.rs:ToolDefinition]
- `StreamEvent`, `StopReason` [stream.rs:StreamEvent, stream.rs:StopReason]
- Clients: `AnthropicClient` [anthropic.rs:AnthropicClient], `OpenAiClient` [openai.rs:OpenAiClient], `OpenAiCompatClient` [openai_compat.rs:OpenAiCompatClient], `GeminiClient` [gemini.rs:GeminiClient], `ClaudeCodeClient` [claude_code.rs:ClaudeCodeClient], `CodexClient` [codex.rs:CodexClient]
- `create_client(...)` (crate) [mod.rs:create_client]; `RetryingLlmClient`, `ConcurrencyLimitedLlmClient` (crate) [mod.rs:RetryingLlmClient, mod.rs:ConcurrencyLimitedLlmClient]; `is_retryable(err)` [mod.rs:is_retryable]
- `registry::{ProviderEntry, ClientConfig, registry(), lookup(provider), from_str_loose(s), all_canonical_names()}` [registry.rs:registry]
- `pricing::{ModelPricing, lookup(provider, model)}` [pricing.rs:lookup]
- `sse_parser::{SseJsonParser, BaseSseParser, ToolBufferContext}` [sse_parser.rs:BaseSseParser]
- CLI helpers: `CliLineParser`, `cli_event_stream`, `filter_tools_for_cli`, `forwarded_tools_for_call`, `sanitized_child_env`, `subscription_proxy` [cli_subprocess.rs:cli_event_stream]
- `start_mcp_server`, `start_mcp_tools_server`, `resolve_binary_path` (crate) [mod.rs:start_mcp_server]
- [dialects](./dialects/EGM.md) - text-based tool calling (Gemma, Qwen) and DeepSeek reasoning handling

## Invariants
- Stream size guards: line buffer 10 MB, per-tool JSON 10 MB, total tool JSON 50 MB, at most 100 active tool buffers [mod.rs:MAX_LINE_BUFFER, mod.rs:MAX_TOOL_JSON, mod.rs:MAX_TOTAL_TOOL_JSON, mod.rs:MAX_ACTIVE_TOOL_BUFFERS]
- Only `LlmRateLimit`, `LlmOverloaded`, and `Http` errors are retryable [mod.rs:is_retryable]; backoff starts at 1 s, doubles with up to 50% jitter, and server retry hints are capped at 90 s [mod.rs:RetryingLlmClient]
- CLI subprocess backends get an environment with secret-named variables removed [cli_subprocess.rs:sanitized_child_env]
- `subscription_proxy` uses the Swarm `pt_` proxy token, never a provider OAuth credential, and returns `None` when base or token is empty [cli_subprocess.rs:subscription_proxy]
- Anthropic requests pin `ANTHROPIC_API_VERSION` [anthropic.rs:ANTHROPIC_API_VERSION]
- OpenAI-compatible text-tool transcripts truncate strings to 2,000 chars [openai_compat.rs:MAX_TEXT_TOOL_TRANSCRIPT_STRING_CHARS]

## Side Effects
- Outbound HTTPS to provider APIs [anthropic.rs:AnthropicClient, openai.rs:OpenAiClient, gemini.rs:GeminiClient]
- Spawns `claude` / `codex` subprocesses [claude_code.rs:ClaudeCodeClient, codex.rs:CodexClient] and starts a loopback MCP server for them [mod.rs:start_mcp_server]
- Reads `CODEX_HOME` / `HOME` for Codex state [codex.rs:CodexClient]; runs `which` to resolve binaries [mod.rs:resolve_binary_path]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
