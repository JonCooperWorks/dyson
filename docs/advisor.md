# Advisor Pattern

Pair a cost-effective executor with a stronger advisor model.  The executor
drives the task; when it hits a complex decision it consults the advisor.

Based on [Anthropic's advisor strategy](https://claude.com/blog/the-advisor-strategy),
which introduced the `advisor_20260301` tool type in the Messages API.  Dyson
extends this to work across all providers: when both models are Anthropic the
native API feature is used; otherwise Dyson spawns a subagent.

**Key files:**
- `src/advisor/mod.rs` -- `Advisor` trait, `NativeAnthropicAdvisor`, `create_advisor()`
- `src/advisor/generic.rs` -- `GenericAdvisor`, `AdvisorTool`
- `src/controller/mod.rs` -- `smartest_model` parsing and advisor wiring in `build_agent()`
- `src/llm/anthropic.rs` -- `api_tool_injections` appended to the tools array

---

## Configuration

`smartest_model` uses `provider/model` format.  The provider name references
an entry from the `"providers"` map.  Everything after the first `/` is the
model identifier passed to that provider's API.

```json
{
  "providers": {
    "claude": { "type": "anthropic", "models": ["claude-sonnet-4-20250514"] }
  },
  "agent": {
    "provider": "claude",
    "model": "claude-sonnet-4-20250514",
    "smartest_model": "claude/claude-opus-4-6"
  }
}
```

Cross-provider advisors work the same way:

```json
{
  "providers": {
    "claude": { "type": "anthropic", "models": ["claude-sonnet-4-20250514"] },
    "openrouter": { "type": "openrouter", "models": ["anthropic/claude-opus-4"] }
  },
  "agent": {
    "provider": "claude",
    "smartest_model": "openrouter/anthropic/claude-opus-4"
  }
}
```

The advisor is **skipped** when:
- `smartest_model` is absent
- The advisor resolves to the same provider type and model as the executor

---

## Two Paths

`build_agent()` in the controller parses `smartest_model`, resolves the
provider client via `ClientRegistry::get()`, and calls `create_advisor()`.
The factory checks both provider types to pick the implementation.

### Native Anthropic (both executor and advisor are Anthropic)

Injects an `advisor_20260301` entry into the Anthropic API request.  The
advisor runs server-side inside a single `/v1/messages` call.  Dyson's
agent loop never sees it.

```
Agent::new()
  NativeAnthropicAdvisor
    bind()             → no-op
    tools()            → []
    api_tool_entries() → [{"type":"advisor_20260301","model":"...","max_uses":3}]
      ↓
  Stored in CompletionConfig.api_tool_injections
      ↓
AnthropicClient::stream()
  tools_json = [bash, read_file, ...] ++ api_tool_injections
  POST /v1/messages
      ↓
Anthropic API handles advisor internally
  Executor consults advisor mid-generation
  Advisor tokens billed separately
  Dyson sees a normal streaming response
```

### Generic (executor or advisor is not Anthropic)

Registers a Dyson-side `advisor` tool that spawns a child agent with the
parent's tools, sandbox, and workspace — same mechanism as `SubagentTool`.

```
Agent::new()
  1. Build tool_registry from skills
  2. Collect inherited tools from registry
  3. advisor.bind(sandbox, workspace, inherited_tools)
     → GenericAdvisor creates AdvisorTool
  4. advisor.tools() → [AdvisorTool]
     → registered as "advisor" in tool_registry
      ↓
Runtime: executor calls the advisor tool
      ↓
AdvisorTool::run()
  Spawns a child Agent:
    model:      smartest_model (advisor provider's client)
    sandbox:    same Arc as parent
    workspace:  same Arc as parent
    tools:      parent's tools (Arc clones)
    max_iter:   15
    max_tokens: 8192
    depth:      parent + 1
  Child runs a full agent loop (can read files, search, etc.)
  Returns advice as ToolOutput::success(text)
      ↓
Parent agent loop continues with advice as tool_result
```

---

## Comparison

| | Native Anthropic | Generic |
|---|---|---|
| Where advisor runs | Anthropic's servers | Dyson child agent |
| Visible to agent loop | No | Yes (tool_use / tool_result) |
| Advisor has tools | No (reasons over conversation context) | Yes (full parent toolset) |
| Extra conversation turns | 0 | 1 tool_use + 1 tool_result |

The generic path is more capable: the advisor can investigate the codebase
before giving advice.  The native path is cheaper and lower latency.

---

## Advisor Trait

```rust
pub trait Advisor: Send + Sync {
    fn bind(&mut self, sandbox: Arc<dyn Sandbox>,
            workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,
            inherited_tools: Vec<Arc<dyn Tool>>) {}
    fn api_tool_entries(&self) -> Vec<serde_json::Value> { vec![] }
    fn tools(&self) -> Vec<Arc<dyn Tool>> { vec![] }
}
```

**Lifecycle:**

1. Controller parses `smartest_model` as `provider/model`, resolves the
   provider client, checks skip rules, calls `create_advisor()`.
2. `create_advisor()` returns `NativeAnthropicAdvisor` (both Anthropic) or
   `GenericAdvisor` (otherwise).
3. `Agent::new()` calls `bind()` with the parent's sandbox, workspace, and
   tools.  Generic uses this to build `AdvisorTool`; native ignores it.
4. `tools()` and `api_tool_entries()` are called to register the advisor in
   the tool registry and/or `CompletionConfig.api_tool_injections`.

After step 4 the advisor is consumed.  Results live in `tool_registry` and
`CompletionConfig` — the advisor itself is not stored on the `Agent`.

---

## Credits

The advisor pattern and `advisor_20260301` tool type were designed by
[Anthropic](https://anthropic.com) and introduced in
[The Advisor Strategy](https://claude.com/blog/the-advisor-strategy).
Dyson's native path uses their API feature directly; the generic path extends
the concept to non-Anthropic providers using Dyson's subagent infrastructure.
