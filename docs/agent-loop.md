# Agent Loop

The agent loop is the core runtime of Dyson.  It orchestrates the
conversation: send messages to the LLM, stream the response, detect tool
calls, execute them through the sandbox, feed results back, repeat.

**Key files:**
- `src/agent/mod.rs` — `Agent` struct, `run()`, `execute_tool_call()`
- `src/agent/stream_handler.rs` — `process_stream()`, `ToolCall`

---

## Agent Struct

```rust
pub struct Agent {
    client: Box<dyn LlmClient>,
    sandbox: Box<dyn Sandbox>,
    skills: Vec<Box<dyn Skill>>,
    tools: HashMap<String, Arc<dyn Tool>>,
    tool_definitions: Vec<ToolDefinition>,
    system_prompt: String,
    config: CompletionConfig,
    max_iterations: usize,
    messages: Vec<Message>,
    tool_context: ToolContext,
}
```

| Field | Purpose |
|-------|---------|
| `client` | Streams completions from Anthropic or OpenAI |
| `sandbox` | Gates every tool call (Allow/Deny/Redirect) |
| `skills` | Retained for lifecycle management (`on_unload` on shutdown) |
| `tools` | Flat lookup by tool name — `Arc` shared with skills |
| `tool_definitions` | Sent to the LLM so it knows available tools |
| `system_prompt` | Base prompt + all skill prompt fragments, composed at construction |
| `config` | Model name, max_tokens, temperature |
| `max_iterations` | Hard limit on LLM turns per `run()` call (default: 20) |
| `messages` | Conversation history — persists across `run()` calls |
| `tool_context` | Working directory, env vars, cancellation token |

### Construction

`Agent::new()` does three things:

1. **Flatten tools** — iterates all skills, clones `Arc<dyn Tool>` pointers
   into the `tools` HashMap and builds `tool_definitions` from each tool's
   name, description, and input schema.

2. **Compose system prompt** — starts with the base prompt from
   `AgentSettings`, then appends each skill's `system_prompt()` fragment
   with `\n\n` separators.

3. **Build tool context** — captures the current working directory and
   creates a `CancellationToken`.

---

## The Loop

```rust
pub async fn run(&mut self, user_input: &str, output: &mut dyn Output) -> Result<String>
```

Each call to `run()` appends the user message to the conversation history,
then loops:

```
1. STREAM
     stream = client.stream(messages, system_prompt, tools, config)
     (assistant_msg, tool_calls) = process_stream(stream, output)
     messages.push(assistant_msg)

2. CHECK
     if tool_calls.is_empty() → break (LLM is done)

3. EXECUTE
     for each tool_call:
       decision = sandbox.check(name, input, ctx)

       match decision:
         Allow { input }             → tool.run(input, ctx)
                                       sandbox.after(name, input, &mut output)
         Deny { reason }             → ToolOutput::error(reason)
         Redirect { name2, input2 }  → tools[name2].run(input2, ctx)
                                       sandbox.after(name2, input2, &mut output)

       messages.push(tool_result(call.id, output))

4. LOOP
     Back to step 1 — LLM sees tool results on next iteration
```

### Internal-tools providers (Claude Code)

Some providers run their own internal agent loop with built-in tools.  Claude
Code, for example, has Bash, Read, Write, Edit, etc. and executes them inside
the `claude -p` subprocess.  The streaming output includes `ToolUseStart` and
`ToolUseComplete` events for these internal tool calls, but Dyson must NOT
re-execute them — they already ran.

The `LlmClient` trait exposes `handles_tools_internally()` (default `false`).
When a provider returns `true`:

1. **Tool definitions are not sent** — the provider has its own tools, so
   Dyson passes an empty `&[]` instead of `self.tool_definitions`.
2. **The loop breaks after one iteration** — even if `tool_calls` is non-empty,
   the agent treats it like a text-only response and breaks.  ToolUse stream
   events are still rendered to output (so the user sees what the provider is
   doing), but Dyson does not attempt to execute them.

Without this check, Dyson would try to re-execute every internal tool call,
fail (the tools aren't in Dyson's registry), push error results, and spawn
another subprocess — repeating up to `max_iterations` times.

### Iteration limit

The `max_iterations` guard prevents infinite loops.  If the LLM keeps
requesting tools without converging, the agent stops after `max_iterations`
turns and emits a warning.  Each "turn" is one LLM call + tool execution.

### Conversation persistence

The `messages` field is **not** cleared between `run()` calls.  This means
multi-turn conversations work naturally — the LLM has full context from
previous turns.  In interactive mode, the user types multiple messages and
the conversation accumulates.

---

## Stream Handler

```rust
pub async fn process_stream(
    stream: Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>,
    output: &mut dyn Output,
) -> Result<(Message, Vec<ToolCall>)>
```

The stream handler is the bridge between the raw `StreamEvent` stream from
the LLM client and the structured data the agent loop needs.

### What it does

1. **Consumes events** from the stream one by one
2. **Renders to output** — `TextDelta` → `output.text_delta()`,
   `ToolUseStart` → `output.tool_use_start()`, etc.
3. **Accumulates content blocks** — text deltas become `ContentBlock::Text`,
   tool use events become `ContentBlock::ToolUse`
4. **Collects tool calls** — `ToolUseComplete` events are collected as
   `ToolCall { id, name, input }` structs

### Text flushing

Text arrives as many small `TextDelta` events.  The handler accumulates them
into a buffer.  When a non-text event arrives (e.g., `ToolUseStart`) or the
message completes, the buffer is flushed as a `ContentBlock::Text`.  This
preserves the interleaving order: text block, tool_use block, text block.

### ToolCall struct

```rust
pub struct ToolCall {
    pub id: String,      // matches the LLM's tool_use block ID
    pub name: String,    // tool name (e.g., "bash")
    pub input: Value,    // parsed JSON input
}
```

The agent loop uses these to look up the tool, run it through the sandbox,
and build a `tool_result` message with the matching `id`.

---

## Tool Execution Flow

A single tool call goes through this sequence:

```
ToolCall { id: "call_1", name: "bash", input: {"command": "ls"} }
  │
  ▼
sandbox.check("bash", {"command":"ls"}, ctx)
  │
  ├─ Allow { input: {"command":"ls"} }
  │    │
  │    ▼
  │  tools["bash"].run({"command":"ls"}, ctx)
  │    │
  │    ▼
  │  ToolOutput { content: "Cargo.toml\nsrc/", is_error: false }
  │    │
  │    ▼
  │  sandbox.after("bash", {"command":"ls"}, &mut output)
  │    │
  │    ▼
  │  output.tool_result(&output)
  │    │
  │    ▼
  │  Message::tool_result("call_1", "Cargo.toml\nsrc/", false)
  │
  ├─ Deny { reason: "blocked by policy" }
  │    │
  │    ▼
  │  Message::tool_result("call_1", "Denied by sandbox: blocked by policy", true)
  │
  └─ Redirect { tool_name: "s3_read", input: {"key": "ls"} }
       │
       ▼
     tools["s3_read"].run({"key":"ls"}, ctx)
       ...same flow as Allow...
```

### Invariant: every tool_use gets a tool_result

The Anthropic API rejects messages where a `tool_use` block in an assistant
message has no corresponding `tool_result`.  Dyson ensures this invariant by
always producing a `tool_result` — even for denied calls (where the deny
reason becomes the error content) and for infrastructure failures (where the
error message becomes the content with `is_error: true`).

---

## Error Recovery

| Error source | What happens |
|-------------|-------------|
| `LlmClient.stream()` fails | `Err` propagates up to `run()` caller |
| `StreamEvent::Error` in stream | `process_stream` returns `Err` immediately |
| `tool.run()` returns `Err(DysonError)` | Converted to `ToolOutput::error(e.to_string())` — LLM sees it |
| `sandbox.check()` returns `Err` | `Err` propagates up (infrastructure failure) |
| Max iterations reached | Warning emitted, loop exits, last text returned |

The key insight: tool-level errors are **not** fatal.  They're reported back
to the LLM as error `tool_result` blocks, and the LLM can decide to retry,
try a different approach, or explain the failure to the user.

---

See also: [Architecture Overview](architecture-overview.md) ·
[LLM Clients](llm-clients.md) · [Sandbox](sandbox.md)
