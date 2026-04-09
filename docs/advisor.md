# Advisor Pattern

The advisor pattern pairs a cost-effective executor model with a more capable
advisor model.  The executor drives the task end-to-end; when it hits a complex
decision, it consults the advisor for strategic guidance.

This is based on [Anthropic's advisor strategy](https://claude.com/blog/the-advisor-strategy),
which introduced the `advisor_20260301` tool type in the Messages API.  Dyson
extends the idea to work across all providers: when both models are Anthropic
the native API feature is used; otherwise Dyson spawns a subagent.

**Key files:**
- `src/advisor/mod.rs` -- `Advisor` trait, `NativeAnthropicAdvisor`, `create_advisor()` factory
- `src/advisor/generic.rs` -- `GenericAdvisor`, `AdvisorTool` (subagent implementation)
- `src/config/mod.rs` -- `AgentSettings.smartest_model`
- `src/config/loader.rs` -- JSON parsing for `smartest_model`
- `src/llm/mod.rs` -- `CompletionConfig.api_tool_injections`
- `src/llm/anthropic.rs` -- injection into the Anthropic request body
- `src/agent/mod.rs` -- `Agent::new()` advisor binding and tool registration
- `src/agent/tests.rs` -- advisor unit tests

---

## Configuration

Add `smartest_model` to the agent config:

```json
{
  "agent": {
    "provider": "claude",
    "model": "claude-sonnet-4-20250514",
    "smartest_model": "claude-opus-4-6"
  }
}
```

When `smartest_model` is absent, the advisor is disabled and behavior is
identical to before.

---

## Two Paths

The `Advisor` trait abstracts over two fundamentally different mechanisms.
The factory function `create_advisor()` picks the right one based on the
executor's provider.

### Native Anthropic (executor is Anthropic)

Uses the `advisor_20260301` tool type from
[Anthropic's advisor strategy](https://claude.com/blog/the-advisor-strategy).
The advisor runs entirely server-side inside a single `/v1/messages` request.

```
┌──────────────────────────────────────────────┐
│  Agent::new()                                │
│                                              │
│  NativeAnthropicAdvisor                      │
│    bind()  → no-op                           │
│    tools() → [] (no Dyson tool)              │
│    api_tool_entries() →                      │
│      [{"type": "advisor_20260301",           │
│        "name": "advisor",                    │
│        "model": "claude-opus-4-6",           │
│        "max_uses": 3}]                       │
│                                              │
│  Stored in config.api_tool_injections        │
└──────────────────┬───────────────────────────┘
                   │
                   ▼
┌──────────────────────────────────────────────┐
│  AnthropicClient::stream()                   │
│                                              │
│  tools_json = [bash, read_file, ...]         │
│  tools_json.extend(api_tool_injections)      │
│  tools_json = [..., advisor_20260301 entry]  │
│                                              │
│  POST /v1/messages with stream: true         │
└──────────────────┬───────────────────────────┘
                   │
                   ▼
┌──────────────────────────────────────────────┐
│  Anthropic API (server-side)                 │
│                                              │
│  Sonnet generates. Hits a hard decision.     │
│  Internally consults Opus for guidance.      │
│  Opus responds. Sonnet continues.            │
│                                              │
│  Dyson never sees the advisor call.          │
│  Advisor tokens billed separately.           │
└──────────────────────────────────────────────┘
```

Dyson's agent loop is completely unaware the advisor exists.  No `tool_use`
events, no `tool_result` messages.  The only thing Dyson does is append a
JSON entry to the tools array.

### Generic (executor is not Anthropic)

Registers a Dyson-side `advisor` tool that spawns a child agent -- the same
mechanism as `SubagentTool`.  The child agent inherits the parent's tools,
sandbox, and workspace.

```
┌──────────────────────────────────────────────┐
│  Agent::new()                                │
│                                              │
│  1. Build tool_registry from skills          │
│     → {bash, read_file, write_file, ...}     │
│                                              │
│  2. Collect inherited_tools from registry    │
│                                              │
│  3. advisor.bind(sandbox, workspace, tools)  │
│     → GenericAdvisor creates AdvisorTool     │
│       with same sandbox, workspace, tools    │
│                                              │
│  4. advisor.tools() → [Arc<AdvisorTool>]     │
│     → registered as "advisor" in registry    │
│                                              │
│  5. advisor.api_tool_entries() → []          │
└──────────────────┬───────────────────────────┘
                   │
                   ▼
┌──────────────────────────────────────────────┐
│  Runtime: executor calls the advisor tool    │
│                                              │
│  ToolUseComplete {                           │
│    name: "advisor",                          │
│    input: { "query": "Should I use a         │
│      trait object or an enum here?" }        │
│  }                                           │
└──────────────────┬───────────────────────────┘
                   │
                   ▼
┌──────────────────────────────────────────────┐
│  AdvisorTool::run()                          │
│                                              │
│  Spawns a child Agent:                       │
│    model:     advisor model (smartest_model)  │
│    sandbox:   same Arc as parent             │
│    workspace: same Arc as parent             │
│    tools:     same as parent (Arc clones)    │
│    max_iter:  15                             │
│    depth:     parent.depth + 1               │
│    prompt:    advisor guidance prompt         │
│                                              │
│  Child runs a full agent loop:               │
│    → reads files, searches code              │
│    → reasons about the architecture          │
│    → returns advice as final text            │
│                                              │
│  → ToolOutput::success(advice_text)          │
└──────────────────┬───────────────────────────┘
                   │
                   ▼
┌──────────────────────────────────────────────┐
│  Parent agent loop continues                 │
│                                              │
│  Advice injected as tool_result.             │
│  Executor reads it, makes its decision,      │
│  continues the original task.                │
└──────────────────────────────────────────────┘
```

---

## Comparison

| | Native Anthropic | Generic |
|---|---|---|
| Where advisor runs | Anthropic's servers | Dyson child agent |
| Visible to agent loop | No | Yes (tool_use / tool_result) |
| Advisor has tools | No (reasons over conversation context) | Yes (full parent toolset) |
| Cost | Advisor tokens at advisor rates | Full completion at advisor model rates |
| Latency | Lower (single request) | Higher (separate agent loop) |
| Extra conversation turns | 0 | 1 tool_use + 1 tool_result |

The generic path is more powerful in one sense: the advisor can investigate
the codebase (read files, search code, run commands) before giving advice,
while the native Anthropic advisor only reasons over the existing conversation
context.

---

## Advisor Trait

```rust
pub trait Advisor: Send + Sync {
    /// Bind to parent resources.  Called from Agent::new() after the
    /// tool registry is built.  No-op for native advisors.
    fn bind(
        &mut self,
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,
        inherited_tools: Vec<Arc<dyn Tool>>,
    ) {}

    /// Raw JSON entries for the API tools array (native path).
    fn api_tool_entries(&self) -> Vec<serde_json::Value> { vec![] }

    /// Dyson-side tools to register (generic path).
    fn tools(&self) -> Vec<Arc<dyn Tool>> { vec![] }
}
```

### Lifecycle

1. **`create_advisor()`** in the controller -- picks `NativeAnthropicAdvisor`
   or `GenericAdvisor` based on the executor's provider.

2. **`bind()`** in `Agent::new()` -- called after the tool registry is built
   from skills.  Passes the parent's sandbox, workspace, and flattened tool
   list.  The `GenericAdvisor` uses this to construct its `AdvisorTool`.
   The `NativeAnthropicAdvisor` ignores it (default no-op).

3. **`tools()`** -- returns Dyson-side tools to register.  Generic returns
   `[AdvisorTool]`; native returns `[]`.

4. **`api_tool_entries()`** -- returns raw JSON for the API request body.
   Native returns the `advisor_20260301` entry; generic returns `[]`.

After these four steps, the advisor's work is done.  The results live in
`tool_registry` (the advisor tool) and `config.api_tool_injections` (the
API entries).  The advisor itself is not stored on the Agent.

---

## Credits

The advisor pattern and the `advisor_20260301` tool type were designed by
[Anthropic](https://anthropic.com) and introduced in
[The Advisor Strategy](https://claude.com/blog/the-advisor-strategy).
Dyson's native Anthropic path uses their API feature directly; the generic
path extends the concept to non-Anthropic providers using Dyson's existing
subagent infrastructure.
