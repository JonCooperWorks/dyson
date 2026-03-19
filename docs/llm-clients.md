# LLM Clients

Dyson supports multiple LLM providers through the `LlmClient` trait.  Each
provider handles request serialization, SSE parsing, and tool call
accumulation internally — the agent loop sees only a stream of `StreamEvent`s.

**Key files:**
- `src/llm/mod.rs` — `LlmClient` trait, `CompletionConfig`, `ToolDefinition`
- `src/llm/stream.rs` — `StreamEvent`, `StopReason`
- `src/llm/anthropic.rs` — Anthropic Messages API (Claude models)
- `src/llm/openai.rs` — OpenAI Chat Completions API (GPT, Codex, Claude Code, Ollama, etc.)

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
    /// Default: false.  Claude Code overrides to true.
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

Both providers implement the same `LlmClient` trait, but their SSE formats
differ significantly:

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

### SSE Event Flow

```
POST /v1/messages (stream: true)
  ↓
event: message_start
  data: {"type":"message_start","message":{"id":"msg_...","role":"assistant"}}

event: content_block_start
  data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
  data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}
  → StreamEvent::TextDelta("Hello")

event: content_block_stop
  data: {"type":"content_block_stop","index":0}

event: content_block_start
  data: {...,"content_block":{"type":"tool_use","id":"call_1","name":"bash"}}
  → StreamEvent::ToolUseStart { id: "call_1", name: "bash" }

event: content_block_delta
  data: {...,"delta":{"type":"input_json_delta","partial_json":"{\"command\""}}
  → StreamEvent::ToolUseInputDelta("{\"command\"")
  → (also accumulated in internal buffer)

event: content_block_stop
  data: {"type":"content_block_stop","index":1}
  → parse accumulated JSON → StreamEvent::ToolUseComplete { id, name, input }

event: message_delta
  data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}
  → StreamEvent::MessageComplete { stop_reason: ToolUse }
```

### SSE Parser

The `SseParser` struct handles three concerns:

1. **Line buffering** — bytes from reqwest can split anywhere (mid-line,
   mid-UTF8-character).  The parser buffers until complete `\n`-delimited
   lines are available.

2. **SSE protocol** — extracts `data:` lines, ignores `event:` lines (the
   event type is determined from the JSON `type` field instead).

3. **Tool use accumulation** — tracks a `HashMap<usize, ToolUseBuffer>` keyed
   by content block index.  Each `input_json_delta` appends to the buffer.
   On `content_block_stop`, the accumulated JSON is parsed and
   `ToolUseComplete` is emitted.

### Message Serialization

`Message::to_anthropic_value()` handles the conversion:

| Internal | Anthropic wire format |
|----------|----------------------|
| `Role::User` + `Text` | `{"role":"user","content":[{"type":"text","text":"..."}]}` |
| `Role::Assistant` + `ToolUse` | `{"role":"assistant","content":[{"type":"tool_use",...}]}` |
| `Role::User` + `ToolResult` | `{"role":"user","content":[{"type":"tool_result",...}]}` |

---

## OpenAI Client

`OpenAiClient` in `src/llm/openai.rs`.

Works with any OpenAI-compatible endpoint:
- **OpenAI** — `https://api.openai.com` (default)
- **Codex CLI** — local server endpoint
- **Claude Code** — OpenAI-compatible proxy
- **Ollama** — `http://localhost:11434`
- **Together** — `https://api.together.xyz`
- **vLLM** — `http://localhost:8000`

### SSE Event Flow

```
POST /v1/chat/completions (stream: true)
  ↓
data: {"choices":[{"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}
  → StreamEvent::TextDelta("Hello")

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1",
        "function":{"name":"bash","arguments":""}}]},"finish_reason":null}]}
  → StreamEvent::ToolUseStart { id: "call_1", name: "bash" }

data: {"choices":[{"delta":{"tool_calls":[{"index":0,
        "function":{"arguments":"{\"command\":"}}]},"finish_reason":null}]}
  → StreamEvent::ToolUseInputDelta("{\"command\":")

...more argument fragments...

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}
  → flush accumulated tool calls → StreamEvent::ToolUseComplete(s)
  → StreamEvent::MessageComplete { stop_reason: ToolUse }

data: [DONE]
```

### Key Differences from Anthropic

1. **Tool calls are separate from content** — OpenAI puts them in
   `delta.tool_calls[]`, not in the content array.  Tool calls are indexed
   by position in the array, not by content block index.

2. **Tool calls flush on finish_reason** — Anthropic emits `content_block_stop`
   per block.  OpenAI accumulates all tool calls and flushes them when
   `finish_reason: "tool_calls"` arrives.

3. **Tool results use role "tool"** — Anthropic puts tool results in
   `role: "user"` messages.  OpenAI has a dedicated `role: "tool"` with a
   `tool_call_id` field.  The `message_to_openai()` function handles this
   conversion.

### Message Serialization

`message_to_openai()` in `src/llm/openai.rs`:

| Internal | OpenAI wire format |
|----------|-------------------|
| `Role::User` + `Text` | `{"role":"user","content":"..."}` |
| `Role::Assistant` + `ToolUse` | `{"role":"assistant","content":null,"tool_calls":[...]}` |
| `Role::User` + `ToolResult` | `{"role":"tool","tool_call_id":"...","content":"..."}` |

---

## Claude Code Client

`ClaudeCodeClient` in `src/llm/claude_code.rs`.

Unlike Anthropic and OpenAI, Claude Code is not a raw API — it's a full agent
with its own tool-use loop.  Dyson spawns `claude -p --output-format stream-json`
as a subprocess and streams the output.

### Key difference: internal tool execution

Claude Code has built-in tools (Bash, Read, Write, Edit, Grep, etc.) and
executes them inside its own subprocess.  The streaming output includes
ToolUse events for these internal calls, but they are **informational only**.
Dyson displays them to the user (so they can see what Claude Code is doing)
but does not re-execute them.

This is controlled by `handles_tools_internally()` returning `true`.  The
agent loop checks this flag and:
- Skips sending Dyson's tool definitions (Claude Code has its own)
- Breaks after one iteration regardless of tool_calls in the stream

Without this, Dyson would see the internal tool calls, try to execute them
(failing because they're not in Dyson's tool registry), and loop up to
`max_iterations` times — spawning a new `claude -p` process each iteration.

### Conversation history

The `claude -p` command is stateless — each invocation is a fresh session.
For multi-turn context, Dyson formats the entire conversation history into
a single text prompt via `format_prompt()`.

---

## Provider Selection

The provider is selected via CLI flags or config:

```bash
# Anthropic (default)
dyson --dangerous-no-sandbox "hello"

# OpenAI
dyson --dangerous-no-sandbox --provider openai "hello"

# Codex CLI local server
dyson --dangerous-no-sandbox --provider openai --base-url http://localhost:3000 "hello"

# Ollama
dyson --dangerous-no-sandbox --provider openai --base-url http://localhost:11434 "hello"
```

Or in `dyson.toml`:

```toml
[agent]
provider = "openai"          # "anthropic" or "openai"
base_url = "http://localhost:11434"
model = "llama3.1"
```

The API key is resolved from the provider-specific environment variable:
- Anthropic: `ANTHROPIC_API_KEY`
- OpenAI: `OPENAI_API_KEY`

---

## Adding a New Provider

1. Create `src/llm/my_provider.rs`
2. Implement `LlmClient` — the only method is `stream()`
3. Add a `MyProvider` variant to `LlmProvider` in `src/config/mod.rs`
4. Wire it up in `main.rs`'s client construction match

The stream must emit `StreamEvent`s in the correct order.  The stream handler
doesn't care about provider-specific details — it just pattern-matches on
events.

---

See also: [Architecture Overview](architecture-overview.md) ·
[Agent Loop](agent-loop.md) · [Tools & Skills](tools-and-skills.md)
