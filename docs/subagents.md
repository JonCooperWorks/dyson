# Subagents

Subagents are child agents that the parent LLM can invoke as tools.  Each
subagent has its own LLM client (potentially a different model/provider), its
own system prompt, and its own conversation history.  When invoked, the subagent
runs to completion and returns its final text as a tool result.

**Key files:**
- `src/skill/subagent.rs` — `SubagentTool`, `SubagentSkill`, `CaptureOutput`, `FilteredSkill`
- `src/config/mod.rs` — `SubagentSkillConfig`, `SubagentAgentConfig`
- `src/config/loader.rs` — JSON parsing for subagent config
- `src/skill/mod.rs` — Two-phase `create_skills()` factory
- `tests/subagent_eval.rs` — Integration tests

---

## Why Subagents?

Different tasks benefit from different models.  A Claude parent might delegate
research to a GPT subagent, or a fast model might delegate complex reasoning
to a slower, more capable one.  Subagents enable this delegation pattern while
maintaining security (shared sandbox) and memory (shared workspace).

---

## Architecture

```
Parent Agent (e.g., Claude Sonnet)
  │
  ├── bash, read_file, ...          ← normal tools
  ├── research_agent (SubagentTool) ← spawns child on invocation
  │     │
  │     ▼
  │   Child Agent (e.g., GPT-4o)
  │     ├── bash, read_file, ...    ← inherited from parent
  │     └── (runs to completion)
  │     │
  │     ▼
  │   returns final text → ToolOutput
  │
  └── code_review_agent (SubagentTool) ← another subagent
```

A subagent is **just a Tool**.  When the parent LLM calls it:

1. `SubagentTool::run()` checks the recursion depth
2. Creates a fresh `LlmClient` for the subagent's provider/model
3. Wraps inherited parent tools in a `FilteredSkill`
4. Constructs a child `Agent` with its own conversation history
5. Runs the child to completion using `CaptureOutput`
6. Returns the captured text as `ToolOutput::success()`

---

## Configuration

Add subagents to your `dyson.json`:

```json
{
  "providers": {
    "claude": {
      "type": "anthropic",
      "models": ["claude-sonnet-4-20250514"],
      "api_key": { "resolver": "insecure_env", "name": "ANTHROPIC_API_KEY" }
    },
    "gpt": {
      "type": "openai",
      "models": ["gpt-4o"],
      "api_key": { "resolver": "insecure_env", "name": "OPENAI_API_KEY" }
    }
  },
  "skills": {
    "builtin": {},
    "subagents": [
      {
        "name": "research_agent",
        "description": "Research specialist for in-depth web research tasks.",
        "system_prompt": "You are a research specialist.",
        "provider": "gpt",
        "max_iterations": 15,
        "tools": ["bash", "web_search", "read_file"]
      },
      {
        "name": "code_review_agent",
        "description": "Code review specialist.",
        "system_prompt": "You are an expert code reviewer.",
        "provider": "claude",
        "max_iterations": 10
      }
    ]
  }
}
```

### Subagent Config Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `name` | yes | — | Tool name the parent uses to invoke this subagent |
| `description` | yes | — | Shown to the parent LLM so it knows when to delegate |
| `system_prompt` | yes | — | System prompt for the child agent |
| `provider` | yes | — | Key into `providers` map (e.g., `"claude"`, `"gpt"`) |
| `model` | no | provider default | Override the model within the provider |
| `max_iterations` | no | 10 | Maximum agent loop iterations for the child |
| `max_tokens` | no | 4096 | Max tokens per LLM response |
| `tools` | no | all parent tools | Tool name filter (see below) |

### Tool Inheritance

Subagents inherit the parent's loaded tools (builtins, MCP, local skills) via
`Arc<dyn Tool>` clones — no duplication, no reconnecting to MCP servers.

- **`tools` omitted**: subagent gets **all** parent tools (minus other subagent tools)
- **`tools: ["bash", "read_file"]`**: only those named tools are visible
- Unknown tool names are silently ignored (e.g., an MCP tool that failed to load)

---

## Design Decisions

### 1. Shared Sandbox

Parent and child share the same sandbox via `Arc<dyn Sandbox>`.  This is
non-negotiable — a subagent must not bypass the parent's security policy.
If the parent's sandbox denies `rm -rf /`, so does the child's.

### 2. Shared Workspace

Subagents share the parent's workspace so they can read/write the same memory
files.  This enables collaboration between parent and child.

### 3. Conversation Isolation

Each subagent invocation starts with a fresh conversation.  The child's
internal messages never leak into the parent's history — only the final text
does.  This keeps the parent's context window clean.

### 4. Recursion Depth Limit

`ToolContext` carries a `depth` counter (max = 3, configurable via
`MAX_SUBAGENT_DEPTH`).  Additionally, subagent tools are excluded from
children's tool sets during two-phase construction, preventing direct
recursion.

### 5. Output Capture

`CaptureOutput` implements `Output` by collecting text into a `String`.
Tool events (tool_use_start, tool_result) are logged for debugging but not
included in the captured text — only the LLM's natural language output
matters for the parent.

### 6. Two-Phase Skill Construction

`create_skills()` uses two phases:

1. **Phase A**: Load all non-subagent skills (builtin, MCP, local)
2. **Phase B**: Flatten loaded tools, construct `SubagentSkill` with those tools

This avoids the chicken-and-egg problem: subagent tools need the parent's
tools to exist first, but they're also part of the skill list that feeds
the parent.

---

## Components

### CaptureOutput

```rust
pub struct CaptureOutput { text: String }
```

Collects `text_delta` events.  Tool events are logged but discarded.
Use `capture.text()` to get the accumulated result.

### FilteredSkill

```rust
struct FilteredSkill { tools: Vec<Arc<dyn Tool>> }
```

Lightweight `Skill` wrapper around pre-loaded tools.  No lifecycle hooks
needed — tools are already initialized by the parent's skills.

### SubagentTool

```rust
pub struct SubagentTool {
    config: SubagentAgentConfig,
    provider: LlmProvider,
    api_key: Credential,
    sandbox: Arc<dyn Sandbox>,
    workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,
    inherited_tools: Vec<Arc<dyn Tool>>,
}
```

The `Tool` impl that spawns a child `Agent` per invocation.  Input schema:

```json
{
  "task": "string (required) — what the subagent should do",
  "context": "string (optional) — background information"
}
```

### SubagentSkill

```rust
pub struct SubagentSkill {
    tools: Vec<Arc<dyn Tool>>,
    system_prompt: String,
}
```

Bundles `SubagentTool` instances into a `Skill`.  Contributes a system prompt
fragment listing available subagents so the parent LLM knows when to delegate.

---

## Testing

### Unit Tests (in `src/skill/subagent.rs`)

- `CaptureOutput`: text collection, tool event handling, flush, file sends
- `SubagentTool`: name/schema metadata, depth limit enforcement, missing task error
- `FilteredSkill`: tool exposure
- `SubagentSkill`: system prompt generation, unknown provider handling
- `filter_tools`: all/named/unknown/empty filter cases

### Integration Tests (in `tests/subagent_eval.rs`)

- Child agent returns result via `CaptureOutput`
- Parent and child share the same sandbox (recording sandbox)
- Child conversation is isolated from parent's messages
- Depth propagation to child `ToolContext`

Run all tests:

```bash
cargo test
```

Run only subagent tests:

```bash
cargo test subagent
```
