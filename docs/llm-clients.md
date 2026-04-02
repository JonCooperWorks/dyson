# LLM Clients

Dyson supports multiple LLM providers through the `LlmClient` trait.  Each
provider handles request serialization, SSE parsing, and tool call
accumulation internally — the agent loop sees only a stream of `StreamEvent`s.

**Key files:**
- `src/llm/mod.rs` — `LlmClient` trait, `CompletionConfig`, `ToolDefinition`, shared utilities
- `src/llm/stream.rs` — `StreamEvent`, `StopReason`
- `src/llm/anthropic.rs` — Anthropic Messages API (Claude models)
- `src/llm/openai.rs` — OpenAI Chat Completions API (GPT, Ollama, etc.)
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

All four providers implement the same `LlmClient` trait.  Anthropic and OpenAI
are API-based; Claude Code and Codex are CLI-subprocess-based.

| Aspect | Anthropic | OpenAI | Claude Code | Codex |
|--------|-----------|--------|-------------|-------|
| Transport | HTTP API | HTTP API | CLI subprocess | CLI subprocess |
| Auth | `x-api-key` header | `Bearer` token | CLI's stored auth | CLI's stored auth |
| API key needed? | Yes | Yes | No | No |
| Tool execution | Dyson | Dyson | Internal | Internal |
| `handles_tools_internally` | `false` | `false` | `true` | `true` |

### API Clients (Anthropic vs OpenAI)

| Aspect | Anthropic | OpenAI |
|--------|-----------|--------|
| Endpoint | `/v1/messages` | `/v1/chat/completions` |
| Auth header | `x-api-key: <key>` | `Authorization: Bearer <key>` |
| System prompt | Separate `system` field | Message with role `"system"` |
| Tool results | Role `"user"` + `tool_result` blocks | Role `"tool"` + `tool_call_id` |
| Tool calls | Content blocks with type `"tool_use"` | Separate `tool_calls` array |
| Tool input streaming | `input_json_delta` in content_block_delta | `function.arguments` fragments |
| Block lifecycle | Explicit `start` / `delta` / `stop` events | Delta objects in `choices[0]` |
| Stream end | `message_stop` event | `data: [DONE]` sentinel |
| Stop reasons | `end_turn`, `tool_use`, `max_tokens` | `stop`, `tool_calls`, `length` |

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
- **Ollama** — `http://localhost:11434`
- **Together** — `https://api.together.xyz`
- **vLLM** — `http://localhost:8000`

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

## Claude Code Client

`ClaudeCodeClient` in `src/llm/claude_code.rs`.

A full agent, not a raw API. Dyson spawns `claude -p --output-format stream-json` as a subprocess. Claude Code has built-in tools (Bash, Read, Write, Edit, etc.) and executes them internally — Dyson displays ToolUse events but does not re-execute them (`handles_tools_internally() = true`).

**Workspace via MCP:** Dyson starts an in-process MCP server with bearer token auth and passes it to Claude Code via `--mcp-config`, giving it access to workspace tools. See [Tool Forwarding over MCP](tool-forwarding-over-mcp.md).

**History:** `claude -p` is stateless — Dyson formats conversation history into a single prompt via `format_prompt()`.

---

## Codex Client

`CodexClient` in `src/llm/codex.rs`.

Same pattern as Claude Code — spawns `codex exec --json` as a subprocess with `handles_tools_internally() = true`. Uses `--full-auto` by default; `--dangerously-bypass-approvals-and-sandbox` only when Dyson's `--dangerous-no-sandbox` is set. Workspace MCP and stateless history work the same way.

---

## Thinking / Reasoning Tokens

Some models emit reasoning tokens (Anthropic's extended thinking, OpenAI's o-series). Captured as `StreamEvent::ThinkingDelta`, logged at `debug` level, **not** sent to output or included in conversation history. Inspect with `RUST_LOG=debug`.

---

## Provider Selection

Select via `--provider` CLI flag or `agent.provider` in `dyson.json`. API keys resolve from env vars (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`); CLI providers need none.

See [Adding a Provider](adding-a-provider.md) for the 3-step process.

---

See also: [Architecture Overview](architecture-overview.md) ·
[Agent Loop](agent-loop.md) · [Tools & Skills](tools-and-skills.md)
