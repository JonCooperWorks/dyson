# Dyson

A streaming AI agent loop in Rust, built to understand how these things actually work — and how to secure them.

> **Educational project.** I'm an AppSec engineer building this to demystify AI agents — how they work, how to secure them, and how to deploy them responsibly. If you need a production agent, see [the projects that inspired this](#inspired-by). If you want to understand what's happening inside the loop, read on.

## Why

AI agents run tools in a loop — reading files, executing commands, making network requests. If you're going to trust one with access to your system, you should understand exactly what's happening inside that loop.

Dyson is a from-scratch implementation of the core agent pattern: **LLM streams a response → detects tool calls → executes them → feeds results back → repeats**. Every component is a trait, every decision point is a hook, and every file is annotated. The goal isn't to replace Claude Code or Cursor — it's to make the loop legible and the security boundaries obvious.

## What you learn by reading this codebase

- **Streaming** — SSE parsing, partial JSON accumulation, token-by-token delivery
- **Tool calling** — `tool_use` blocks, execution, `tool_result` messages back to the LLM
- **Security boundaries** — every tool call passes through a `Sandbox` that can allow, deny, or redirect
- **MCP** — MCP servers are just another skill implementation; the agent loop doesn't know they exist
- **Provider differences** — Anthropic and OpenAI have different SSE formats, message schemas, and tool calling conventions

## What it does

Dyson is a fully functional agent — not just a learning exercise. It streams completions, calls tools, manages context, and persists state across conversations.

- **Six LLM backends** — Anthropic, OpenAI, OpenRouter, Ollama Cloud, Claude Code, and Codex. Swap providers with a flag; the agent loop doesn't care which one it's talking to.
- **Sandboxed tool execution** — Every tool call passes through a `Sandbox` that can allow, deny, or redirect it. The default `PolicySandbox` wraps bash in OS-level sandboxes (macOS Apple Containers / Linux bubblewrap) and enforces per-tool capability policies.
- **MCP integration** — Connect to any MCP server (stdio or HTTP). MCP servers are just another skill; the agent loop doesn't know they exist.
- **Workspace persistence** — Conversations, memory, and a knowledge base are stored in SQLite. Memory is tiered: always-in-context, FTS5 search, and journals. The knowledge base supports document ingestion with full-text search.
- **Context compaction** — When the context window fills up, Dyson runs a five-phase summarization to compress history while preserving key information. Pre-compaction history is rotated for fine-tuning preservation.
- **Subagents** — Spawn child agents with different models and tool sets. They inherit parent tools and report back.
- **Multi-channel** — Terminal REPL and Telegram bot run concurrently. Add new channels by implementing the `Controller` trait.
- **Dependency analysis** — Tool calls are grouped by resource dependencies and executed in parallel when safe, sequentially when not.
- **MCP server mode** — Dyson can also expose itself as an MCP server over HTTP with bearer token auth, forwarding its workspace tools to other agents.

## The sandbox is the point

The `Sandbox` trait is the key abstraction. Every tool call goes through it:

```
LLM says: tool_use("bash", {"command": "rm -rf /"})
  → sandbox.check("bash", input, ctx)
    → Allow { input }      — run the tool (with possibly rewritten input)
    → Deny { reason }      — block it, tell the LLM why
    → Redirect { tool, input } — transparently swap to a different tool
  → sandbox.after(name, input, &mut output)
    → inspect/redact/audit the output
```

Sandbox implementations:

- **PolicySandbox** — wraps bash commands in the OS's native sandbox (macOS [Apple Containers](https://github.com/apple/container) / Linux [bubblewrap](https://github.com/containers/bubblewrap)). Enforces per-tool capability policies at both the application level and OS level. Truncates oversized tool outputs.
- **DangerousNoSandbox** — passthrough, no restrictions (development only)

Sandbox prerequisites:
- **macOS (Apple Silicon):** `brew install container`
- **Linux:** `apt install bubblewrap`

Dyson refuses to start without the OS sandbox binary unless `--dangerous-no-sandbox` is passed.

## Footprint

**4.9 MB** on macOS, **6.3 MB** on Linux, **~16 MB RSS** at idle, ~30 direct dependencies. The dependency tree is small enough to audit by hand — fewer crates, less supply-chain surface, and a codebase one person can read end to end.

## Architecture

```
User input
  → Controller (terminal, Telegram, etc.)
    → Agent.run()
      → LlmClient.stream(messages, system_prompt, tools)
        → Stream<StreamEvent>
          → TextDelta("Hello")     → output immediately
          → ToolUseComplete{...}   → Sandbox.check() → Tool.run() → Sandbox.after()
          → MessageComplete        → done, or loop if tools were called
```

Everything is a trait: `LlmClient` (streaming from any provider), `Tool` (a single capability), `Skill` (a bundle of tools with lifecycle hooks), `Sandbox` (gate every tool call), `Controller` (own the I/O lifecycle), and `SecretResolver` (resolve secrets from any backend).

## Providers

Six LLM backends, selectable via `--provider` or config:

| Provider | Config value | API key | Notes |
|----------|-------------|---------|-------|
| Anthropic | `"anthropic"` | `ANTHROPIC_API_KEY` | Default. Full streaming + structured tool calling |
| OpenAI | `"openai"` | `OPENAI_API_KEY` | Also works with vLLM, Together, rLLM, etc. via `base_url` |
| OpenRouter | `"openrouter"` | `OPENROUTER_API_KEY` | 200+ models via OpenAI-compatible API |
| Ollama Cloud | `"ollama-cloud"` | `OLLAMA_API_KEY` | Cloud-hosted models on ollama.com |
| Claude Code | `"claude-code"` | None (uses stored creds) | Spawns the `claude` CLI. Zero config. Claude Code handles tools |
| Codex | `"codex"` | None (uses stored creds) | Spawns the `codex` CLI. OpenAI Codex agent loop |

Adding a new provider is a 3-step process — see [Adding a Provider](docs/adding-a-provider.md).

## Quick start

```bash
# Anthropic (default provider)
export ANTHROPIC_API_KEY="sk-ant-..."
cargo run -- --dangerous-no-sandbox "what files are in this directory?"

# Claude Code (no API key needed)
cargo run -- --dangerous-no-sandbox --provider claude-code "hello"

# Ollama Cloud
export OLLAMA_API_KEY="..."
cargo run -- --dangerous-no-sandbox --provider ollama-cloud --model llama3.3 "hello"

# Local model (vLLM, rLLM, local Ollama, etc.)
cargo run -- --dangerous-no-sandbox --provider openai --base-url http://localhost:9000 --model llama-3.2-3b-instruct "hello"
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

With web search, MCP, and Telegram:

```json
{
  "config_version": 2,
  "providers": {
    "claude": { "type": "claude-code", "models": ["sonnet"] }
  },
  "agent": { "provider": "claude" },
  "web_search": { "provider": "searxng", "base_url": "https://searx.be" },
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
| [Subagents](docs/subagents.md) | Child agents with different models, tool inheritance, delegation |

## Tests

```bash
cargo test
```

700+ tests covering the full stack — SSE parsing, sandbox decisions, config loading, workspace persistence, and the agent loop with mock LLM clients.

## Inspired by

- **[OpenClaw](https://github.com/openclaw/openclaw)** — Multi-channel AI assistant. Dyson's controller architecture and workspace format come from OpenClaw.
- **[Hermes Agent](https://github.com/NousResearch/hermes-agent)** — Self-improving agent from [Nous Research](https://nousresearch.com/). Dyson's memory system is modeled after Hermes.
- **[zeroclaw](https://github.com/zeroclaw-labs/zeroclaw)** — Minimal agent focused on resource consumption.
