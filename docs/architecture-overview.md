# Architecture Overview

Dyson is a streaming, composable AI agent loop in Rust.  An LLM streams
tool calls in a loop until it has an answer.  Everything else — MCP servers,
skills, local tools — plugs into that loop through traits.

---

## End-to-End Data Flow

```
User types "list the files"
       │
       ▼
  ┌──────────┐
  │  main.rs  │  Parse CLI args, load config, create Agent
  └─────┬────┘
        │  agent.run("list the files", &mut output)
        ▼
  ┌──────────────────────────────────────────────────┐
  │  Agent Loop (agent/mod.rs)                       │
  │                                                  │
  │  messages = [User("list the files")]             │
  │                                                  │
  │  for iteration in 0..max_iterations:             │
  │    ┌─────────────────────────────────┐           │
  │    │  LlmClient.stream(messages,     │           │
  │    │    system_prompt, tools, config) │           │
  │    └───────────────┬─────────────────┘           │
  │                    │ Stream<StreamEvent>          │
  │                    ▼                             │
  │    ┌─────────────────────────────────┐           │
  │    │  stream_handler::process_stream │           │
  │    │    TextDelta → output.text()    │           │
  │    │    ToolUseComplete → ToolCall   │           │
  │    └───────────────┬─────────────────┘           │
  │                    │ (Message, Vec<ToolCall>)     │
  │                    ▼                             │
  │    if tool_calls.is_empty() → break              │
  │                                                  │
  │    for each tool_call:                           │
  │      ┌───────────────────────────────┐           │
  │      │  Sandbox.check(name, input)   │           │
  │      │    Allow → tool.run(input)    │           │
  │      │    Deny  → error result       │           │
  │      │    Redirect → other_tool.run  │           │
  │      └──────────────┬────────────────┘           │
  │                     │ ToolOutput                 │
  │                     ▼                            │
  │      messages.push(tool_result)                  │
  │                                                  │
  │    loop → LLM sees tool results next iteration   │
  └──────────────────────────────────────────────────┘
        │
        ▼
  Final text returned to user
```

---

## Component Hierarchy

```
Agent
  ├── client: Box<dyn LlmClient>          ← Anthropic, OpenAI, Claude Code, or Codex
  ├── sandbox: Box<dyn Sandbox>            ← gates every tool call
  ├── skills: Vec<Box<dyn Skill>>          ← own tools + lifecycle
  │     ├── BuiltinSkill
  │     │     └── tools: Vec<Arc<dyn Tool>>
  │     │           └── BashTool
  │     ├── McpSkill                       ← tools discovered from MCP servers
  │     └── LocalSkill                     ← prompt-only, from workspace skills/
  ├── tools: HashMap<name, Arc<dyn Tool>>  ← flat lookup (shared Arcs)
  ├── tool_definitions: Vec<ToolDefinition> ← sent to the LLM
  ├── system_prompt: String                ← base + skill fragments
  ├── config: CompletionConfig             ← model, max_tokens, temp
  ├── messages: Vec<Message>               ← conversation history
  ├── tool_context: ToolContext            ← working dir, env, cancel
  ├── limiter: ToolLimiter                 ← per-turn rate limiting
  ├── formatter: ResultFormatter           ← structured output formatting
  ├── token_budget: TokenBudget            ← cumulative token usage tracking
  └── compaction_config: CompactionConfig  ← context window management
```

---

## Core Traits

Dyson has five core traits.  The agent loop only interacts through these
interfaces — it never knows the concrete types behind them.

| Trait | File | Purpose |
|-------|------|---------|
| `LlmClient` | `src/llm/mod.rs` | Stream a completion from any LLM provider |
| `Tool` | `src/tool/mod.rs` | A single callable capability (bash, file read, MCP remote) |
| `Skill` | `src/skill/mod.rs` | A bundle of tools with lifecycle hooks and prompt fragments |
| `Sandbox` | `src/sandbox/mod.rs` | Gate tool calls: allow, deny, or redirect |
| `Output` | `src/controller/mod.rs` | Render agent events to the user (terminal, JSON, etc.) |

### Trait relationships

```
Skill owns → Arc<dyn Tool>
Agent borrows → Arc<dyn Tool> (cloned into flat HashMap)
Agent calls → Sandbox.check() before every Tool.run()
Agent calls → LlmClient.stream() each iteration
Agent calls → Output methods for display
```

---

## Message Types

All conversation state flows through three types defined in `src/message.rs`:

```rust
enum Role { User, Assistant }

enum ContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
}

struct Message {
    role: Role,
    content: Vec<ContentBlock>,
}
```

| Message shape | Role | Content blocks | When created |
|---------------|------|----------------|--------------|
| User input | `User` | `[Text]` | User types a message |
| LLM text response | `Assistant` | `[Text]` | LLM responds without tools |
| LLM tool request | `Assistant` | `[Text, ToolUse, ToolUse, ...]` | LLM wants to use tools |
| Tool result | `User` | `[ToolResult]` | After tool executes |

**Why tool results use `Role::User`:** The Anthropic API requires tool results
in user-role messages with `tool_result` content blocks.  OpenAI uses a
separate `"tool"` role.  Dyson stores them as `User` internally and each
`LlmClient` handles the provider-specific serialization.

---

## Streaming Architecture

Streaming is not optional — it's the foundation of Dyson's UX and architecture.

```
LlmClient.stream()
       │
       ▼
  Pin<Box<dyn Stream<Item = Result<StreamEvent>>>>
       │
       ├── TextDelta("Hello")        → print immediately
       ├── TextDelta(" world")       → print immediately
       ├── ToolUseStart { id, name } → display tool marker
       ├── ToolUseInputDelta(json)   → (accumulated in LLM client)
       ├── ToolUseComplete { ... }   → ready for execution
       └── MessageComplete { stop }  → end of this LLM turn
```

### StreamEvent enum

| Variant | Produced by | Consumed by |
|---------|-------------|-------------|
| `TextDelta(String)` | SSE parser | stream_handler → Output.text_delta() |
| `ToolUseStart { id, name }` | SSE parser | stream_handler → Output.tool_use_start() |
| `ToolUseInputDelta(String)` | SSE parser | (logging only — accumulation in LLM client) |
| `ToolUseComplete { id, name, input }` | SSE parser (on block stop) | stream_handler → ToolCall |
| `MessageComplete { stop_reason }` | SSE parser | stream_handler → flush text |
| `Error(DysonError)` | SSE parser | stream_handler → return Err |

`ToolUseComplete` is synthetic — the LLM client accumulates `ToolUseInputDelta` fragments and emits `ToolUseComplete` when the block finishes, keeping the stream handler stateless.

---

## Error Handling

All errors flow through a single `DysonError` enum (`src/error.rs`):

```rust
enum DysonError {
    Llm(String),                        // API rejection, rate limit
    Tool { tool: String, message: String }, // a tool failed
    Mcp { server: String, message: String }, // MCP transport/protocol
    Config(String),                     // bad config
    Io(#[from] std::io::Error),         // filesystem
    Http(#[from] reqwest::Error),       // HTTP transport
    Json(#[from] serde_json::Error),    // JSON parse
    Cancelled,                          // Ctrl-C
}
```

`DysonError` = tool couldn't run (infra failure, propagates up). `ToolOutput { is_error: true }` = tool ran but failed (LLM sees it, can retry).

---

## Key Design Decisions

- **Stream everything** — text tokens go to the user immediately; no buffering
- **MCP is not special** — `McpSkill` is just another `Skill` impl; Dyson also serves as an MCP server
- **Skills own tools** — `Arc<dyn Tool>` shared via flat HashMap for O(1) dispatch
- **Sandbox gates everything** — mandatory; `DangerousNoSandbox` is an explicit opt-out
- **Config is parse-once** — all formats merge into one `Settings` struct
- **Async all the way down** — tools, skills, sandbox, LLM client all async on tokio
- **The UI is a trait** — `Output` abstracts rendering (terminal, JSON, TUI, websocket)

---

See also: [Agent Loop](agent-loop.md) · [LLM Clients](llm-clients.md) ·
[Tools & Skills](tools-and-skills.md) · [Sandbox](sandbox.md) ·
[Tool Execution Pipeline](tool-execution-pipeline.md) ·
[Tool Forwarding over MCP](tool-forwarding-over-mcp.md)
