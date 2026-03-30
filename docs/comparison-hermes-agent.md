# Dyson vs Hermes Agent (Nous Research)

A side-by-side comparison of Dyson and
[Hermes Agent](https://github.com/NousResearch/hermes-agent) — two open-source
agent frameworks that share DNA (Dyson's memory system is directly inspired by
Hermes) but make very different trade-offs.

---

## At a Glance

| Dimension | Dyson | Hermes Agent |
|-----------|-------|--------------|
| **Language** | Rust | Python |
| **License** | MIT | MIT |
| **Primary goal** | Educational — understand + secure agent loops | Practical — self-improving personal agent |
| **GitHub stars** | — | ~16,600 |
| **Release** | Pre-1.0 | v0.3.0 (March 2026) |

---

## Design Philosophy

### Dyson — "Understand and secure the loop"

Dyson is a from-scratch Rust implementation meant to teach how agent loops
work internally and where the security boundaries are. Every component is a
trait, every file is heavily annotated, and the `Sandbox` is the central
abstraction — security gates every tool call. It's an educational framework
first, production framework second.

### Hermes Agent — "The agent that grows with you"

Hermes Agent is a practical, batteries-included AI agent designed for daily
use. Its headline feature is a **self-improving learning loop**: it creates
skills from experience, nudges itself to persist knowledge, searches past
conversations, and builds a model of its user over time. It doubles as a
**data generation pipeline** for training better tool-calling models via RL
(Atropos).

---

## Architecture

### Agent Loop

Both follow the ReAct pattern (Observe → Reason → Act → Loop), but differ
in implementation:

| Aspect | Dyson | Hermes Agent |
|--------|-------|--------------|
| **Loop** | `for iteration in 0..max_iterations` with streaming tool calls | ReAct cycle: Observation → Reasoning → Action |
| **Streaming** | First-class SSE streaming; text tokens delivered immediately | Streaming supported via provider APIs |
| **Tool dispatch** | Every call goes through `Sandbox.check()` before execution | Direct tool execution with container-level isolation |
| **Post-processing** | `Sandbox.after()` hook inspects/redacts/audits output | Skill lifecycle hooks |
| **Max iterations** | Configurable (default 20) | Configurable |

### Core Abstractions

| Dyson Trait | Hermes Equivalent | Notes |
|-------------|-------------------|-------|
| `LlmClient` | Provider system | Both abstract over multiple LLM providers |
| `Tool` | Tool | Both define name + description + JSON schema + run |
| `Skill` | Skill | Bundle of tools + lifecycle hooks + prompt fragments |
| `Sandbox` | Container isolation | **Key difference** — Dyson's sandbox is a trait in the agent loop; Hermes uses execution backends |
| `Controller` | Gateway | Dyson: terminal, Telegram. Hermes: Telegram, Discord, Slack, WhatsApp, Signal, Email, CLI |
| `Workspace` | Workspace / Memory | Both use file-based persistent identity |

---

## LLM Support

| Provider | Dyson | Hermes Agent |
|----------|-------|--------------|
| **Anthropic (Claude)** | Native client | Native (v0.3.0+), also via OpenRouter |
| **OpenAI (GPT)** | Native client | Via OpenAI-compatible API |
| **OpenRouter** | Native client | Native (200+ models) |
| **Local (Ollama/vLLM/llama.cpp)** | Via OpenAI-compatible `base_url` | Native support |
| **Claude Code** | Observe mode (spawns CLI) | — |
| **Codex** | Observe mode (spawns CLI) | — |
| **Nous Portal** | — | Native |
| **Hermes-3 (fine-tuned Llama 3.1)** | Via OpenAI-compatible endpoint | Primary target model |

Dyson has a unique **ToolMode** distinction:
- **Execute** — Dyson runs tool calls itself (Anthropic, OpenAI, OpenRouter)
- **Observe** — Provider runs its own loop; Dyson watches (Claude Code, Codex)

---

## Memory System

Dyson's memory is directly inspired by Hermes Agent's. Both use a tiered
architecture with agent-curated persistent files.

### Dyson — Three Tiers

| Tier | Content | Access |
|------|---------|--------|
| **1 — Always-in-Context** | `MEMORY.md` (2,200 chars), `USER.md` (1,375 chars) | Injected into every system prompt |
| **2 — Searchable Archive** | `memory/notes/*.md` | SQLite FTS5 via `memory_search` tool |
| **3 — Historical Journals** | `memory/YYYY-MM-DD.md` | Yesterday + today auto-included |

### Hermes Agent — Five Layers

Hermes extends the concept with five memory layers, including:
- Always-in-context identity files (SOUL.md, MEMORY.md, USER.md)
- Searchable conversation archive
- Daily journals
- Skill-local memory
- Cross-session user modeling that deepens over time

### Context Compaction

Both frameworks automatically compress conversation history when approaching
the model's context window, using the same five-phase algorithm:

| Phase | Description |
|-------|-------------|
| **1 — Tool output pruning** | Replace old tool results with placeholders (no LLM call) |
| **2 — Region identification** | Protect head (first N messages) and tail (recent token budget) |
| **3 — Structured summarisation** | Summarise the middle via LLM (Goal / Progress / Decisions / Files / Next Steps) |
| **4 — Reassembly** | Head + `[Context Summary]` + tail |
| **5 — Orphan repair** | Fix broken tool_use / tool_result pairs at boundaries |

Dyson's implementation supports iterative summaries — when compacting a
conversation that was already compacted, it merges the old and new summaries.

### Key Difference

Hermes Agent **actively learns** — it creates skills from experience, improves
them during use, and builds a deepening model of the user. Dyson provides the
memory infrastructure but relies on the LLM's own initiative (with periodic
nudges) to maintain it.

---

## Security / Sandboxing

This is Dyson's biggest differentiator.

### Dyson — Trait-Level Sandbox

Every tool call passes through `Sandbox.check()` before execution:

```
LLM says: tool_use("bash", {"command": "rm -rf /"})
  → sandbox.check("bash", input, ctx)
    → Allow { input }           — run (possibly rewritten input)
    → Deny { reason }           — block, tell LLM why
    → Redirect { tool, input }  — transparently swap to different tool
  → sandbox.after(name, input, &mut output)
    → inspect / redact / audit the output
```

Sandbox implementations:
- **PolicySandbox** — app-level JSON policy (network, file_write, path restrictions)
- **OsSandbox** — OS-native: bubblewrap (Linux) / Seatbelt (macOS)
- **CompositeSandbox** — chain multiple sandboxes
- **DangerousNoSandbox** — passthrough (development only, requires CLI flag)

The `Redirect` capability is unique — you can transparently route file reads
to S3, bash commands to a container, or writes through an approval tool.

### Hermes Agent — Execution Backend Isolation

Hermes relies on execution backends for security:
- Docker containers with read-only root, dropped capabilities, namespace isolation
- SSH, Modal, Daytona, Singularity backends
- Container hardening at the infrastructure level

### Comparison

| Aspect | Dyson | Hermes Agent |
|--------|-------|--------------|
| **Granularity** | Per-tool-call policy (allow/deny/redirect) | Per-container isolation |
| **Policy language** | JSON config per tool name (glob patterns) | Container/backend config |
| **Redirect** | Yes — transparently swap tools | No |
| **Output auditing** | `sandbox.after()` hook | — |
| **OS sandboxing** | bwrap / Seatbelt wrapping bash | Docker / execution backend |

Dyson offers finer-grained, application-level security. Hermes offers
stronger infrastructure-level isolation via containers.

---

## Skills / Tools

| Aspect | Dyson | Hermes Agent |
|--------|-------|--------------|
| **Built-in tools** | bash, file I/O, web search, memory search, workspace tools | 40+ skills (MLOps, GitHub, research, etc.) |
| **Skill format** | Trait impl (Rust) or workspace `.md` files | Python files, agentskills.io format |
| **Auto-creation** | No — skills are defined by developers | Yes — agent creates skills from experience |
| **Ecosystem** | MCP servers | ClawHub, LobeHub, GitHub, agentskills.io |
| **MCP support** | Full (MCP servers are just another Skill) | Plugin-based |
| **Lifecycle hooks** | `on_load`, `after_tool`, `on_unload` | Similar lifecycle hooks |
| **Skill sharing** | Via MCP protocol | Open agentskills.io format |

### MCP Integration

Dyson treats MCP as a first-class but not special citizen — MCP tools are
wrapped in `McpSkill` and appear in the flat tool lookup just like `bash` or
`read_file`. The agent loop has zero awareness of MCP. Dyson can also act as
an MCP *server* to expose workspace tools to Claude Code.

---

## Interface / Controllers

| Interface | Dyson | Hermes Agent |
|-----------|-------|--------------|
| Terminal / CLI | Yes | Yes |
| Telegram | Yes | Yes |
| Discord | — | Yes |
| Slack | — | Yes |
| WhatsApp | — | Yes |
| Signal | — | Yes |
| Email | — | Yes |
| IDE (VS Code, JetBrains, Zed) | — | Yes (ACP Server) |
| Voice | — | Yes (push-to-talk, Whisper) |

Hermes Agent has significantly broader interface support.

---

## Training / Research

| Aspect | Dyson | Hermes Agent |
|--------|-------|--------------|
| **Data generation** | Not a focus | Core feature — batch trajectory generation |
| **RL training** | — | Atropos RL environments |
| **Export format** | — | ShareGPT for fine-tuning |
| **Tool-call parsers** | — | 11 parsers for any model architecture |
| **Self-evolution** | — | DSPy + GEPA (ICLR 2026 Oral) |

This is unique to Hermes — it's simultaneously an agent framework and a data
pipeline for training better agent models.

---

## Summary

| Strength | Dyson | Hermes Agent |
|----------|-------|--------------|
| **Security model** | Finer-grained, trait-level sandbox with redirect | Infrastructure-level container isolation |
| **Educational value** | Heavily annotated, every design decision explained | Less focus on pedagogy |
| **Self-improvement** | — | Self-creating skills, deepening user model |
| **Interface breadth** | Terminal + Telegram | 7+ messaging platforms + IDE + voice |
| **Ecosystem** | MCP-native | 40+ skills, ClawHub, agentskills.io |
| **Research/training** | — | RL pipelines, trajectory generation, self-evolution |
| **Performance** | Rust — low overhead, async throughout | Python — broader ecosystem access |
| **Model flexibility** | 5 providers + local | 6+ providers + local + Nous Portal |
| **Memory system** | 3-tier (inspired by Hermes) | 5-layer (original design) |
| **Observe mode** | Unique — wrap Claude Code/Codex | — |

**Choose Dyson if** you want to understand agent internals, need fine-grained
per-tool security policies, prefer Rust, or want to build custom sandboxing
logic.

**Choose Hermes Agent if** you want a batteries-included personal agent that
learns and improves over time, need broad interface support, want to generate
training data, or prefer the Python ecosystem.

They're complementary more than competitive — Dyson is the "learn how it
works and lock it down" framework; Hermes is the "use it every day and let it
grow" agent.
