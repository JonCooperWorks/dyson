# Dyson — One Pager

**Dyson** is a streaming AI agent loop written in Rust. It's a from-scratch,
auditable implementation built by an AppSec engineer to demystify how AI agents
actually work and how to secure them. Small footprint (~6–8 MB binary,
~13 MB RSS idle, ~30 direct deps), one person can read it end-to-end.

## Core idea

Everything is a trait: `LlmClient`, `Tool`, `Skill`, `Sandbox`, `Controller`,
`SecretResolver`. The agent loop is:

```
Controller (terminal / telegram / swarm)
  -> Agent.run()
    -> LlmClient.stream(messages, system_prompt, tools)   // SSE
      -> TextDelta            -> stream to user
      -> ToolUseComplete{...} -> Sandbox.check -> Tool.run -> Sandbox.after
      -> MessageComplete      -> done, or loop if tools were called
```

## What it does

- **Streaming** — SSE parsing, partial JSON accumulation, token-by-token output.
- **Sandboxed tool execution** — every tool call passes through a `Sandbox`
  trait (`Allow` / `Deny` / `Redirect`, plus `after()` for output
  redaction/audit). Default `PolicySandbox` wraps bash in the OS sandbox
  (macOS Apple Containers / Linux bubblewrap). Refuses to start without
  a sandbox binary unless `--dangerous-no-sandbox` is set.
- **Workspace persistence** — SQLite with tiered memory, FTS5 search,
  knowledge base, and chat history per channel.
- **Context compaction** — five-phase summarization when the window fills;
  pre-compaction history rotated for fine-tuning.
- **Subagents** — child agents with different models / tool sets, inheriting
  parent tools.
- **Dreaming** — background tasks distill completed work into `SKILL.md`
  files that hot-reload via mtime watching.
- **Multi-channel** — terminal REPL + Telegram bot concurrently; add
  channels via the `Controller` trait.
- **MCP** — MCP servers plug in as skills; the agent loop doesn't know they
  exist. Dyson can also run *as* an MCP server with bearer-token auth.
- **Six providers** — Anthropic, OpenAI (+ vLLM/Together/etc. via `base_url`),
  OpenRouter, Ollama Cloud, Claude Code CLI, Codex CLI.

## The Swarm

A distributed task-routing layer over a **trusted network**. Adding a swarm
controller makes a Dyson both a **worker** (hub can send it tasks) and a
**client** (its agent can dispatch tasks to peers). Participation is symmetric.

**Hub** (`crates/swarm/`, ships as the `swarm` binary):
- In-memory, tokio HTTP server. State is ephemeral — a restart forgets every
  node and in-flight task.
- Endpoints: `/mcp` (JSON-RPC tools), `/swarm/register`, `/swarm/events` (SSE
  push to nodes), `/swarm/heartbeat`, `/swarm/result`, `/swarm/checkpoint`,
  `/swarm/blob/{sha256}`.
- **Signs every task with Ed25519.** Wire format is
  `version(1) || sig(64) || canonical JSON`. V1 = Ed25519, no algorithmic
  agility — bump the version to change the algorithm.
- Blob storage for large payloads (datasets, weights) referenced by SHA-256;
  signatures cover the hashes so tampering breaks verification.

**Node** (any Dyson with `"type": "swarm"` in controllers):
- Probes hardware (CPUs, GPUs via `nvidia-smi`/`system_profiler`, RAM, disk),
  registers with the hub, opens an SSE stream, heartbeats every 15 s.
- Verifies every task signature before executing. Runs via `agent.run()` with
  a cancellation token; emits `swarm_checkpoint` events (per epoch, per
  pipeline stage) forwarded to the hub.
- On disconnect, reconnects with exponential backoff (2 s base, 60 s cap,
  10 attempts).

**Routing: the caller decides.** The hub deliberately does *not* pick which
node runs a task. The LLM calls `list_nodes` (returns hardware, capabilities,
status, `busy_task_id`), reasons about fit, and calls `swarm_dispatch`
(sync, blocks up to 600 s) or `swarm_submit` (async, returns `task_id`). Both
flow through one `TaskStore`, so `swarm_task_status`, `swarm_task_checkpoints`,
`swarm_task_result`, and `swarm_task_cancel` work uniformly.

**Security model.** Ed25519 signing proves a task came from *the hub you
trust*. It does **not** authenticate callers: `/mcp` and `/swarm/register`
are open. On non-localhost binds, TLS is mandatory (manual cert or
Let's Encrypt via TLS-ALPN-01) and node bearer tokens + an optional argon2id
static MCP API key gate traffic. Recommended transports: Tailscale mesh,
SSH port-forward, or public internet behind a firewall restricted to known
peers. The swarm is explicitly designed for **trusted networks**, not the
open internet.

**What v1 doesn't do:** persistence across hub restarts, task queueing
(no eligible node = fast fail), automatic progress scraping, hard-kill of
bash subprocesses on cancel (cancellation is cooperative).
