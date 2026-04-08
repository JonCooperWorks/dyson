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

## Built-in Subagents

Dyson ships with two built-in subagents that are always available.  They use
the `"default"` provider (the parent agent's own model), so they work out of
the box with zero extra configuration.

| Name | Description | Tools | Max Iterations |
|------|-------------|-------|----------------|
| `planner` | Breaks down complex tasks into concrete, ordered implementation steps.  Reads the codebase to understand structure before planning. | `read_file`, `search_files`, `list_files` | 15 |
| `researcher` | Does deep research and summarizes findings.  Can read code, run commands, and search the web. | `bash`, `read_file`, `search_files`, `list_files`, `web_search` | 20 |

The planner is deliberately read-only — it plans but doesn't execute.
The researcher has broader access for thorough investigation.

User-defined subagents in `dyson.json` are appended after built-ins.

---

## Why Subagents?

Different tasks benefit from different models. Subagents enable delegation (e.g., Claude delegates research to GPT) while sharing the parent's sandbox and workspace.

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
2. Creates an `LlmClient` for the subagent's provider/model (wrapped in an unlimited `RateLimitedHandle`)
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
| `provider` | yes | — | Key into `providers` map (e.g., `"claude"`, `"gpt"`), or `"default"` to use the parent's provider |
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

- **Shared sandbox** — parent and child share `Arc<dyn Sandbox>`. Non-negotiable: a subagent cannot bypass the parent's security policy.
- **Shared workspace** — same memory files, enabling collaboration.
- **Conversation isolation** — each invocation starts fresh. Only the final text returns to the parent.
- **Recursion depth limit** — `ToolContext.depth` maxes at 3. Subagent tools are excluded from children's tool sets.
- **Output capture** — `CaptureOutput` collects text only; tool events are logged but not returned.
- **Two-phase construction** — Phase A loads non-subagent skills; Phase B builds `SubagentSkill` from the flattened tool set. Avoids the chicken-and-egg problem.

---

## Components

| Component | Purpose |
|-----------|---------|
| `CaptureOutput` | Collects `text_delta` events; tool events logged but discarded |
| `FilteredSkill` | Lightweight `Skill` wrapper around pre-loaded parent tools |
| `SubagentTool` | `Tool` impl that spawns a child `Agent` per invocation. Input: `{ "task": "...", "context": "..." }` |
| `SubagentSkill` | Bundles `SubagentTool`s into a `Skill` with system prompt listing available subagents |

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
