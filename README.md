# Dyson

A streaming AI agent loop in Rust, built to understand how these things actually work — and how to secure them.

> **Educational project.** I'm an AppSec engineer building this to demystify AI agents — how they work, how to secure them, and how to deploy them responsibly. Trust the loop, read the loop. If you need a production agent, see [the projects that inspired this](#inspired-by).

<p align="center">
  <img src="docs/images/dyson-swarm.png" alt="Dyson Swarm" width="600">
</p>

## What you learn by reading this codebase

- **Streaming** — SSE parsing, partial JSON accumulation, token-by-token delivery
- **Tool calling** — `tool_use` blocks, execution, result messages back to the LLM
- **Security boundaries** — every tool call passes through a `Sandbox`
- **MCP** — MCP servers are just another skill; the agent loop doesn't know they exist
- **Provider differences** — Anthropic and OpenAI have different SSE formats, schemas, and tool conventions

## What it does

Dyson is a fully functional agent — not just a learning exercise.

- **Self-improvement** — background "dream" tasks distill completed work into `SKILL.md` files that hot-reload via mtime watching
- **Sandboxed tool execution** — every tool call passes through a `Sandbox` that can allow, deny, or redirect; the default `PolicySandbox` wraps bash in OS-level sandboxes (macOS Apple Containers / Linux bubblewrap)
- **Workspace persistence** — conversations, memory, and a knowledge base in SQLite with tiered memory and FTS5 search
- **Context compaction** — five-phase summarization when the context window fills; pre-compaction history rotated for fine-tuning
- **Subagents** — child agents with different models and tool sets, inheriting parent tools
- **Multi-channel** — terminal REPL and Telegram bot concurrently; add channels via the `Controller` trait
- **Dependency analysis** — tool calls grouped by resource dependencies, executed in parallel when safe
- **MCP server mode** — expose Dyson as an MCP server over HTTP with bearer token auth
- **Swarm** — distribute tasks across Dyson nodes; see [Swarm (trusted-network only)](#swarm-trusted-network-only)

## The sandbox is the point

The `Sandbox` trait is the key abstraction. Every tool call goes through it:

```
LLM says: tool_use("bash", {"command": "rm -rf /"})
  -> sandbox.check("bash", input, ctx)
    -> Allow { input }      — run the tool (with possibly rewritten input)
    -> Deny { reason }      — block it, tell the LLM why
    -> Redirect { tool, input } — transparently swap to a different tool
  -> sandbox.after(name, input, &mut output)
    -> inspect/redact/audit the output
```

- **PolicySandbox** — wraps bash in the OS's native sandbox (macOS [Apple Containers](https://github.com/apple/container) / Linux [bubblewrap](https://github.com/containers/bubblewrap)). Enforces per-tool capability policies. Truncates oversized outputs.
- **DangerousNoSandbox** — passthrough, no restrictions (development only)

Dyson refuses to start without the OS sandbox binary unless `--dangerous-no-sandbox` is passed.

## Swarm (trusted-network only)

The swarm distributes tasks across Dyson nodes. A central hub signs work with Ed25519; nodes verify before executing.

**Security model:** Ed25519 signing ensures tasks came from the hub you trust. It does **not** authenticate or authorize callers. `/mcp` and `/swarm/register` are open endpoints — anyone who can reach the hub can register as a node or dispatch tasks.

**Recommended transports:** run the hub on a Tailscale mesh or behind an SSH port-forward. TLS is mandatory for off-localhost binds but it encrypts traffic — it does not gate callers. If you deploy the hub on the public internet, firewall it to known peers.

See [docs/swarm.md](docs/swarm.md) for the full architecture, configuration, and task lifecycle.

## Footprint

**6.4 MB** on macOS, **8.3 MB** on Linux, **~12.7 MB RSS** at idle, ~30 direct dependencies. The dependency tree is small enough to audit by hand — fewer crates, less supply-chain surface, and a codebase one person can read end to end.

## Architecture

```
User input
  -> Controller (terminal, Telegram, etc.)
    -> Agent.run()
      -> LlmClient.stream(messages, system_prompt, tools)
        -> Stream<StreamEvent>
          -> TextDelta("Hello")     -> output immediately
          -> ToolUseComplete{...}   -> Sandbox.check() -> Tool.run() -> Sandbox.after()
          -> MessageComplete        -> done, or loop if tools were called
```

Everything is a trait: `LlmClient`, `Tool`, `Skill`, `Sandbox`, `Controller`, `SecretResolver`.

## Providers

Six LLM backends, selectable via `--provider` or config:

| Provider | Config value | API key | Notes |
|----------|-------------|---------|-------|
| Anthropic | `"anthropic"` | `ANTHROPIC_API_KEY` | Default. Full streaming + structured tool calling |
| OpenAI | `"openai"` | `OPENAI_API_KEY` | Also works with vLLM, Together, rLLM, etc. via `base_url` |
| OpenRouter | `"openrouter"` | `OPENROUTER_API_KEY` | 200+ models via OpenAI-compatible API |
| Ollama Cloud | `"ollama-cloud"` | `OLLAMA_API_KEY` | Cloud-hosted models on ollama.com |
| Claude Code | `"claude-code"` | None (uses stored creds) | Spawns the `claude` CLI. Zero config |
| Codex | `"codex"` | None (uses stored creds) | Spawns the `codex` CLI. OpenAI Codex agent loop |

Adding a new provider is a 3-step process — see [Adding a Provider](docs/adding-a-provider.md).

## Quick start

```bash
# Anthropic (default provider)
export ANTHROPIC_API_KEY="sk-ant-..."
cargo run -- --dangerous-no-sandbox "what files are in this directory?"

# Claude Code (no API key needed)
cargo run -- --dangerous-no-sandbox --provider claude-code "hello"
```

`--dangerous-no-sandbox` disables all sandboxes — an explicit acknowledgment that tool calls are unrestricted. Run without arguments for interactive mode. Use `--config` for config files (see [`examples/`](examples/)).

## Configuration

Dyson uses JSON config files (`dyson.json`). See [`examples/`](examples/) for ready-to-use configs, or the full [Configuration](docs/configuration.md) docs.

Minimal:

```json
{
  "config_version": 2,
  "providers": {
    "default": { "type": "anthropic", "models": ["claude-sonnet-4-20250514"] }
  },
  "agent": { "provider": "default" }
}
```

With MCP and Telegram:

```json
{
  "config_version": 2,
  "providers": {
    "claude": { "type": "claude-code", "models": ["sonnet"] }
  },
  "agent": { "provider": "claude" },
  "mcp_servers": {
    "context7": { "url": "https://mcp.context7.com/mcp" }
  },
  "controllers": [
    { "type": "telegram", "bot_token": { "resolver": "insecure_env", "name": "TELEGRAM_API_KEY" } }
  ]
}
```

Secrets can be literal strings or resolver references (`{ "resolver": "insecure_env", "name": "ENV_VAR" }`). See [Secrets](docs/secrets.md).

## Docs

| Document | Covers |
|----------|--------|
| [Architecture Overview](docs/architecture-overview.md) | End-to-end data flow, component hierarchy, core traits |
| [Agent Loop](docs/agent-loop.md) | The streaming loop, tool execution, error recovery |
| [LLM Clients](docs/llm-clients.md) | Anthropic vs OpenAI vs Claude Code, SSE parsing |
| [Tools & Skills](docs/tools-and-skills.md) | Tool trait, Skill trait, MCP skill, adding new tools |
| [Sandbox](docs/sandbox.md) | Allow/Deny/Redirect, PolicySandbox, Apple Containers, bubblewrap |
| [Secrets](docs/secrets.md) | Per-secret resolver routing, InsecureEnvironmentVariable |
| [Tool Execution Pipeline](docs/tool-execution-pipeline.md) | Rate limiting, dependency analysis, result formatting, lifecycle hooks |
| [Configuration](docs/configuration.md) | dyson.json format, provider selection |
| [Adding a Provider](docs/adding-a-provider.md) | 3-step process to add a new LLM provider |
| [Memory](docs/memory.md) | Tiered memory system, FTS5 search, journals |
| [Knowledge Base](docs/knowledge-base.md) | Document storage, FTS5 search, kb/raw + kb/wiki, INDEX.md |
| [Chat Persistence](docs/chat-persistence.md) | ChatHistory trait, per-chat agents |
| [Tool Forwarding over MCP](docs/tool-forwarding-over-mcp.md) | MCP server mode, bearer token auth |
| [Public Agents](docs/public-agents.md) | Group chat agents, AgentMode, tool restriction, SSRF protection |
| [Subagents](docs/subagents.md) | Child agents with different models, tool inheritance, delegation |
| [Dreaming](docs/dreaming.md) | Background cognition — memory consolidation, self-improvement, skill creation |
| [Advisor](docs/advisor.md) | Advisor pattern — consult a stronger model for complex decisions |
| [Swarm](docs/swarm.md) | Distributed task routing over a trusted network — hub, workers, Ed25519-signed dispatch |

## Tests

```bash
cargo test
```

700+ tests covering the full stack — SSE parsing, sandbox decisions, config loading, workspace persistence, and the agent loop with mock LLM clients.

## Inspired by

- **[OpenClaw](https://github.com/openclaw/openclaw)** — Multi-channel AI assistant. Dyson's controller architecture and workspace format come from OpenClaw.
- **[Hermes Agent](https://github.com/NousResearch/hermes-agent)** — Self-improving agent from [Nous Research](https://nousresearch.com/). Dyson's memory system is modeled after Hermes.
- **[zeroclaw](https://github.com/zeroclaw-labs/zeroclaw)** — Minimal agent focused on resource consumption.
