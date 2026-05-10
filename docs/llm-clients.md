# LLM Clients

Dyson supports multiple LLM providers through the `LlmClient` trait.  Each
provider handles request serialization, SSE parsing, and tool call
accumulation internally — the agent loop sees only a stream of `StreamEvent`s.

**Key files:**
- `src/llm/mod.rs` — `LlmClient` trait, `CompletionConfig`, `ToolDefinition`, shared utilities
- `src/llm/stream.rs` — `StreamEvent`, `StopReason`
- `src/llm/anthropic.rs` — Anthropic Messages API (Claude models)
- `src/llm/openai.rs` — OpenAI Chat Completions API (GPT, etc.)
- `src/llm/openai_compat.rs` — OpenAI-compatible endpoints and model dialects
- `src/llm/openrouter.rs` — OpenRouter API (200+ models via OpenAI-compatible endpoint)
- `src/llm/ollama_cloud.rs` — Ollama Cloud API (cloud-hosted models on ollama.com)
- `src/llm/gemini.rs` — Gemini `streamGenerateContent` API
- `src/llm/claude_code.rs` — Claude Code CLI subprocess (no API key needed)
- `src/llm/codex.rs` — Codex CLI subprocess (no API key needed)

---

## LlmClient Trait

```rust
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn stream(
        &self,
        messages: &[Message],
        system: &str,
        tools: &[ToolDefinition],
        config: &CompletionConfig,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>>;

    /// Whether this provider runs its own internal tool-use loop.
    /// Default: false.  Claude Code and Codex override to true.
    fn handles_tools_internally(&self) -> bool { false }
}
```

| Parameter | Purpose |
|-----------|---------|
| `messages` | Conversation history (user, assistant, tool results) |
| `system` | System prompt (passed separately, not as a message) |
| `tools` | Available tools — the LLM decides which to call |
| `config` | Model name, max_tokens, temperature |

Returns a `Stream` of `StreamEvent`s.  The stream ends with
`MessageComplete`.  Errors are either `Err` items in the stream or
`StreamEvent::Error` events.

---

## Provider Comparison

All providers implement the same `LlmClient` trait.

| Provider | Transport | Auth | Tool execution |
|---|---|---|---|
| Anthropic | HTTP API | `x-api-key` | Dyson |
| OpenAI | HTTP API | `Bearer` | Dyson |
| OpenRouter | HTTP API | `Bearer` | Dyson |
| Gemini | HTTP API | `x-goog-api-key` | Dyson |
| Ollama Cloud | HTTP API | `Bearer` | Dyson |
| Claude Code | CLI subprocess | CLI stored auth | Internal |
| Codex | CLI subprocess | CLI stored auth | Internal |

`handles_tools_internally()` is `true` only for Claude Code and Codex. For
those providers, Dyson displays streamed tool events but does not send its own
tool definitions or execute the returned tool calls.

### API Clients

| Aspect | Anthropic | OpenAI-compatible | Gemini |
|---|---|---|---|
| Endpoint | `/v1/messages` | `/v1/chat/completions` | `/v1beta/models/{model}:streamGenerateContent?alt=sse` |
| System prompt | Separate `system` field | Message with role `"system"` | `systemInstruction` |
| Tool results | User-role `tool_result` blocks | Tool-role messages | Function response parts |
| Tool calls | `tool_use` content blocks | `tool_calls` array | Function call parts |
| Tool input streaming | JSON fragments | `function.arguments` fragments | Complete function-call args |
| Stream end | `message_stop` event | `data: [DONE]` sentinel | SSE stream exhaustion |

---

## Anthropic Client

`AnthropicClient` in `src/llm/anthropic.rs`.

### SSE Flow

Anthropic uses explicit content block lifecycle events:

1. `message_start` → `content_block_start` (text or tool_use) → `content_block_delta` (fragments) → `content_block_stop` → `message_delta` (stop_reason)
2. Text deltas → `StreamEvent::TextDelta`
3. Tool use: `content_block_start` → `ToolUseStart`, `input_json_delta` fragments accumulated, `content_block_stop` → `ToolUseComplete` (parsed JSON)

The SSE parser handles line buffering (reqwest can split mid-UTF8), `data:` extraction, and tool use accumulation via `HashMap<usize, ToolUseBuffer>`.

### Message Serialization

| Internal | Anthropic wire format |
|----------|----------------------|
| `User` + `Text` | `{"role":"user","content":[{"type":"text",...}]}` |
| `Assistant` + `ToolUse` | `{"role":"assistant","content":[{"type":"tool_use",...}]}` |
| `User` + `ToolResult` | `{"role":"user","content":[{"type":"tool_result",...}]}` |

---

## OpenAI Client

`OpenAiClient` in `src/llm/openai.rs`.

Works with any OpenAI-compatible endpoint:
- **OpenAI** — `https://api.openai.com` (default)
- **Together** — `https://api.together.xyz`
- **vLLM** — `http://localhost:8000`
- **Local Ollama** — `http://localhost:11434`

### SSE Flow

OpenAI streams `choices[0].delta` objects. Key differences from Anthropic:

- Tool calls live in `delta.tool_calls[]` (not content blocks), indexed by position
- All tool calls flush when `finish_reason: "tool_calls"` arrives (no per-block stop events)
- Stream ends with `data: [DONE]` sentinel

### Message Serialization

| Internal | OpenAI wire format |
|----------|-------------------|
| `User` + `Text` | `{"role":"user","content":"..."}` |
| `Assistant` + `ToolUse` | `{"role":"assistant","tool_calls":[...]}` |
| `User` + `ToolResult` | `{"role":"tool","tool_call_id":"...","content":"..."}` |

---

## OpenRouter Client

`OpenRouterClient` in `src/llm/openrouter.rs`.

Thin wrapper around `OpenAiCompatClient` for [OpenRouter](https://openrouter.ai) — a unified API for 200+ models using the OpenAI Chat Completions format. Adds the default base URL (`https://openrouter.ai/api`) and app attribution headers (`HTTP-Referer`, `X-Title`). Supports dialect-based tool call handling for models that need it (e.g., Gemma).

---

## Gemini Client

`GeminiClient` in `src/llm/gemini.rs`.

Uses Google's `streamGenerateContent` SSE endpoint. Dyson converts internal
messages to Gemini `contents`, sends the system prompt as `systemInstruction`,
and sanitizes JSON Schemas because Gemini supports a smaller schema subset than
OpenAI or Anthropic. Gemini function-call arguments arrive complete rather than
as incremental JSON fragments, so the parser emits `ToolUseComplete` directly
from the streamed part.

---

## Claude Code Client

`ClaudeCodeClient` in `src/llm/claude_code.rs`.

A full agent, not a raw API. Dyson spawns `claude -p --output-format stream-json` as a subprocess. Claude Code has built-in tools (Bash, Read, Write, Edit, etc.) and executes them internally — Dyson displays ToolUse events but does not re-execute them (`handles_tools_internally() = true`).

**Workspace via MCP:** Dyson starts an in-process MCP server with bearer token auth and passes it to Claude Code via `--mcp-config`, giving it access to workspace tools. See [Tool Forwarding over MCP](tool-forwarding-over-mcp.md).

**History:** `claude -p` is stateless — Dyson formats conversation history into a single prompt via `format_prompt()`.

---

## Ollama Cloud Client

`OllamaCloudClient` in `src/llm/ollama_cloud.rs`.

Thin wrapper around `OpenAiCompatClient` for [Ollama Cloud](https://ollama.com) — cloud-hosted models accessible via an OpenAI-compatible API at `https://ollama.com`. Uses `Bearer` token auth with an `OLLAMA_API_KEY`. Supports dialect-based tool call handling for models that need it (e.g., Gemma).

---

## Codex Client

`CodexClient` in `src/llm/codex.rs`.

Same pattern as Claude Code — spawns `codex exec --json` as a subprocess with `handles_tools_internally() = true`. Uses `--full-auto` by default; `--dangerously-bypass-approvals-and-sandbox` only when Dyson's `--dangerous-no-sandbox` is set. Workspace MCP and stateless history work the same way.

---

## Thinking / Reasoning Tokens

Some models emit reasoning tokens (Anthropic's extended thinking, OpenAI's o-series). Captured as `StreamEvent::ThinkingDelta`, logged at `debug` level, **not** sent to output or included in conversation history. Inspect with `RUST_LOG=debug`.

---

## Provider Selection

Select via `--provider` CLI flag or `agent.provider` in `dyson.json`. API keys
resolve from provider config first, then from env vars when no `base_url`
override is set: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`,
`GEMINI_API_KEY`, or `OLLAMA_API_KEY`. CLI providers need none.

See [Adding a Provider](adding-a-provider.md) for the 3-step process.

---

See also: [Architecture Overview](architecture-overview.md) ·
[Agent Loop](agent-loop.md) · [Tools & Skills](tools-and-skills.md)
