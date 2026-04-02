# Agent Loop

The agent loop is the core runtime of Dyson.  It orchestrates the
conversation: send messages to the LLM, stream the response, detect tool
calls, execute them through the sandbox, feed results back, repeat.

**Key files:**
- `src/agent/mod.rs` — `Agent` struct, `run()`, `execute_tool_call()`
- `src/agent/stream_handler.rs` — `process_stream()`, `ToolCall`
- `src/agent/tool_limiter.rs` — `ToolLimiter` (per-turn rate limiting)
- `src/agent/dependency_analyzer.rs` — `DependencyAnalyzer` (parallel vs sequential grouping)
- `src/agent/result_formatter.rs` — `ResultFormatter` (structured LLM-optimized output)
- `src/agent/compaction.rs` — Five-phase context window summarization
- `src/agent/token_budget.rs` — Cumulative token usage tracking
- `src/agent/reflection.rs` — Agent state introspection
- `src/tool_hooks.rs` — `ToolHook` trait (pre/post execution lifecycle hooks)

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
| `client` | Streams completions from any LLM provider (Anthropic, OpenAI, Claude Code, Codex) |
| `sandbox` | Gates every tool call (Allow/Deny/Redirect) |
| `skills` | Retained for lifecycle management (`on_unload` on shutdown) |
| `tools` | Flat lookup by tool name — `Arc` shared with skills |
| `tool_definitions` | Sent to the LLM so it knows available tools |
| `system_prompt` | Base prompt + all skill prompt fragments, composed at construction |
| `config` | Model name, max_tokens, temperature |
| `max_iterations` | Hard limit on LLM turns per `run()` call (default: 20) |
| `messages` | Conversation history — persists across `run()` calls |
| `tool_context` | Working directory, env vars, cancellation token |
| `limiter` | Per-turn tool call rate limiter (`ToolLimiter`) |
| `formatter` | Structured result formatter for LLM-optimized output (`ResultFormatter`) |

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

3. LIMIT
     for each tool_call:
       limiter.check(name)
         Over limit → push error tool_result, skip execution
         Within limit → add to allowed list

4. ANALYZE
     phases = DependencyAnalyzer.analyze(allowed_calls)
       Groups calls into Parallel and Sequential phases
       based on resource conflicts (file paths, git state)

5. EXECUTE (per phase)
     Parallel phases  → futures::join_all(...)
     Sequential phases → one-by-one in order

     Each call goes through the sandbox:
       decision = sandbox.check(name, input, ctx)
       match decision:
         Allow { input }             → tool.run(input, ctx)
                                       sandbox.after(name, input, &mut output)
         Deny { reason }             → ToolOutput::error(reason)
         Redirect { name2, input2 }  → tools[name2].run(input2, ctx)
                                       sandbox.after(name2, input2, &mut output)

6. FORMAT
     formatted = ResultFormatter.format(call, output, duration)
     messages.push(tool_result(call.id, formatted.to_llm_message()))

7. RESET
     limiter.reset_turn()

8. LOOP
     Back to step 1 — LLM sees tool results on next iteration
```

### Internal-tools providers (Claude Code, Codex)

Some providers run their own agent loop with built-in tools. When `handles_tools_internally()` returns `true`:

1. Dyson's tool definitions are **not** sent (provider has its own)
2. The loop **breaks after one iteration** — tool events are displayed but not re-executed

### Iteration limit

`max_iterations` (default 20) prevents infinite loops. Each "turn" = one LLM call + tool execution.

### Conversation persistence

`messages` persists across `run()` calls — multi-turn conversations accumulate naturally.

---

## Stream Handler

```rust
pub async fn process_stream(
    stream: Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>,
    output: &mut dyn Output,
) -> Result<(Message, Vec<ToolCall>)>
```

Bridges raw `StreamEvent`s to structured data:

1. Renders events to `Output` (text deltas, tool markers)
2. Accumulates content blocks (`Text`, `ToolUse`)
3. Collects `ToolCall { id, name, input }` structs from `ToolUseComplete` events

Text deltas are buffered and flushed as a `ContentBlock::Text` when a non-text event arrives, preserving interleaving order.

---

## Tool Execution Flow

```
sandbox.check(name, input, ctx)
  → Allow { input }      → tool.run() → sandbox.after() → tool_result
  → Deny { reason }      → error tool_result (reason as content)
  → Redirect { tool, in} → other_tool.run() → sandbox.after() → tool_result
```

**Invariant:** every `tool_use` gets a `tool_result` — even denied calls and infra failures. The Anthropic API rejects orphaned tool_use blocks.

---

## Error Recovery

| Error source | What happens |
|-------------|-------------|
| `LlmClient.stream()` fails | `Err` propagates up to `run()` caller |
| `StreamEvent::Error` in stream | `process_stream` returns `Err` immediately |
| `tool.run()` returns `Err(DysonError)` | Converted to `ToolOutput::error(e.to_string())` — LLM sees it |
| `sandbox.check()` returns `Err` | `Err` propagates up (infrastructure failure) |
| Max iterations reached | Warning emitted, loop exits, last text returned |

Tool-level errors are **not** fatal — they're reported to the LLM as error `tool_result` blocks, and the LLM can retry or adjust.

---

See also: [Architecture Overview](architecture-overview.md) ·
[LLM Clients](llm-clients.md) · [Sandbox](sandbox.md) ·
[Tool Execution Pipeline](tool-execution-pipeline.md)
