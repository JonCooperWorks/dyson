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
    client: RateLimitedHandle<Box<dyn LlmClient>>,
    sandbox: Arc<dyn Sandbox>,
    skills: Vec<Box<dyn Skill>>,
    tool_registry: ToolRegistry,
    system_prompt: Arc<str>,
    config: CompletionConfig,
    max_iterations: usize,
    max_retries: usize,
    conversation: Conversation,
    tool_context: ToolContext,
    compaction_config: CompactionConfig,
    limiter: ToolLimiter,
    formatter: ResultFormatter,
    tool_hooks: Vec<Box<dyn ToolHook>>,
    dream_handle: DreamHandle,
    history_backend: Option<HistoryBackend>,
    feedback_store: Option<FeedbackStore>,
    transcriber: Option<Arc<dyn Transcriber>>,
}
```

| Field | Purpose |
|-------|---------|
| `client` | Handle to the shared, rate-limited LLM client (from `ClientRegistry`) |
| `sandbox` | Gates every tool call (Allow/Deny/Redirect) |
| `skills` | Retained for lifecycle management (`on_unload` on shutdown) |
| `tool_registry` | Flat lookup, tool definitions, and token-cache view of loaded tools |
| `system_prompt` | Base prompt + all skill prompt fragments, composed at construction |
| `config` | Model name, max_tokens, temperature |
| `max_iterations` | Hard limit on LLM turns per `run()` call (default: 40) |
| `max_retries` | Transient LLM retry budget |
| `conversation` | Messages, turn count, and token budget — persists across `run()` calls |
| `tool_context` | Working directory, env vars, cancellation token |
| `compaction_config` | Context-window compaction thresholds and behaviour |
| `limiter` | Per-turn tool call rate limiter (`ToolLimiter`) |
| `formatter` | Structured result formatter for LLM-optimized output (`ResultFormatter`) |
| `tool_hooks` | Pre/post tool execution hooks |
| `dream_handle` | Background memory, learning, and self-improvement task trigger |
| `history_backend` | Optional rotating snapshot persistence for pre-compaction history |
| `feedback_store` | Optional user-rating store used by dream tasks |
| `transcriber` | Optional audio transcriber for media attachments |

### Construction

`Agent::new()` does three things:

1. **Build the tool registry** — iterates all skills, clones `Arc<dyn Tool>`
   pointers into a flat lookup, and builds tool definitions from each tool's
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
     response = client.stream(messages, system_prompt, system_suffix, tools, config)
     (assistant_msg, tool_calls, output_tokens, stop_reason) =
       process_stream(response.stream, output)
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

### CLI-subprocess providers (Claude Code, Codex)

Some providers run their own agent loop with built-in tools. When the
`StreamResponse` returns `ToolMode::Observe`:

1. Dyson exposes its loaded tools to the subprocess over a per-turn loopback
   MCP server when a workspace is available.
2. Tool events in the provider stream are displayed but not re-executed by
   Dyson.
3. The loop breaks after the observed provider turn.

### Iteration limit

`max_iterations` (default 40) prevents infinite loops. Each "turn" = one LLM
call + tool execution. If the limit is reached while tools are still being
requested, Dyson asks the model for one final tool-free summary before
returning.

### Conversation persistence

Conversation messages persist across `run()` calls — multi-turn conversations
accumulate naturally.

---

## Stream Handler

```rust
pub async fn process_stream(
    stream: Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>,
    output: &mut dyn Output,
) -> Result<(Message, Vec<ToolCall>, usize, StopReason)>
```

Bridges raw `StreamEvent`s to structured data:

1. Renders events to `Output` (text and thinking deltas, tool markers)
2. Accumulates content blocks (`Text`, `Thinking`, `ToolUse`)
3. Collects `ToolCall { id, name, input }` structs from `ToolUseComplete` events
4. Returns output-token count and the final stop reason for budgeting/retry decisions

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
| Max iterations reached | Warning emitted, one final tool-free summary is requested, summary text returned |

Tool-level errors are **not** fatal — they're reported to the LLM as error `tool_result` blocks, and the LLM can retry or adjust.

---

See also: [Architecture Overview](architecture-overview.md) ·
[LLM Clients](llm-clients.md) · [Sandbox](sandbox.md) ·
[Tool Execution Pipeline](tool-execution-pipeline.md)
