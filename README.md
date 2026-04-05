# Dyson

A streaming AI agent loop in Rust, built to understand how these things actually work — and how to secure them.

> **This is an educational project.** I'm an AppSec engineer building this to demystify AI agents — how they work, how to secure them, and how to deploy them responsibly. If you need a production agent, see [the projects that inspired this](#inspired-by). If you want to understand what's happening inside the loop, read on.

## Why

AI agents run tools in a loop. They read your files, execute commands, make network requests. If you're going to trust one with access to your system, you should understand exactly what's happening inside that loop.

Dyson is a from-scratch implementation of the core agent pattern: **LLM streams a response → detects tool calls → executes them → feeds results back → repeats**. Every component is a trait, every decision point is a hook, and every file is annotated explaining *why* it works the way it does. The goal isn't to replace Claude Code or Cursor — it's to make the loop legible and the security boundaries obvious.

## What you learn by reading this codebase

- **Streaming** — SSE parsing, partial JSON accumulation, token-by-token delivery
- **Tool calling** — `tool_use` blocks, execution, `tool_result` messages back to the LLM
- **Security boundaries** — every tool call passes through a `Sandbox` that can allow, deny, or redirect
- **MCP** — MCP servers are just another skill implementation; the agent loop doesn't know they exist
- **Provider differences** — Anthropic and OpenAI have different SSE formats, message schemas, and tool calling conventions

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

- **OsSandbox** — wraps bash commands in the OS's native sandbox (macOS Seatbelt / Linux bubblewrap). Also truncates oversized tool outputs (including MCP results)
- **DangerousNoSandbox** — passthrough, no restrictions (development only)

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

Six core traits:

| Trait | What it does |
|-------|-------------|
| `LlmClient` | Stream completions from any provider (Anthropic, OpenAI, OpenRouter, Ollama Cloud, Claude Code, Codex) |
| `Tool` | A single callable capability (bash, web search, MCP remote tool) |
| `Skill` | A bundle of tools with lifecycle hooks and prompt fragments |
| `Sandbox` | Gate every tool call — allow, deny, redirect, audit |
| `Controller` | Own the input/output lifecycle (terminal REPL, Telegram bot) |
| `SecretResolver` | Resolve secrets from any backend (env vars, Vault, SSM) |

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

`--dangerous-no-sandbox` is required — an explicit acknowledgment that tool calls are unrestricted. Run without arguments for interactive mode. Use `--config` for config files (see [`examples/`](examples/)).

## Configuration

Dyson uses JSON config files (`dyson.json`). See [`examples/`](examples/) for ready-to-use configs, or the full [Configuration](docs/configuration.md) docs.

Minimal:

```json
{ "agent": { "provider": "anthropic", "model": "claude-sonnet-4-20250514" } }
```

With web search, MCP, and Telegram:

```json
{
  "agent": { "provider": "claude-code", "model": "sonnet" },
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

## Project structure

```
src/
  main.rs              CLI entry point
  lib.rs               Module tree
  error.rs             DysonError — unified error type
  message.rs           Message, Role, ContentBlock
  config/              Settings, JSON loader
  tool/                Tool trait + bash, file ops, search, memory, knowledge base, web search, skills, export
  skill/               Skill trait, BuiltinSkill, MCP skill, LocalSkill, SubagentSkill
  sandbox/             Sandbox trait, OsSandbox, DangerousNoSandbox
  secret/              SecretResolver trait, InsecureEnvironmentVariable
  llm/                 LlmClient trait + provider registry + Anthropic, OpenAI, OpenRouter, Ollama Cloud, Claude Code, Codex
  agent/               Streaming loop, compaction, dependency analysis, rate limiting, token tracking
  controller/          Controller trait + terminal, Telegram
  workspace/           Workspace trait, memory store, knowledge base, migrations
examples/              Example dyson.json configs
docs/                  Component documentation
```

Every file has extensive annotations in the style of [rLLM](https://github.com/joncooperworks/rllm) — learning overviews, architecture diagrams, and inline commentary on non-obvious code.

## Docs

| Document | Covers |
|----------|--------|
| [Architecture Overview](docs/architecture-overview.md) | End-to-end data flow, component hierarchy, core traits |
| [Agent Loop](docs/agent-loop.md) | The streaming loop, tool execution, error recovery |
| [LLM Clients](docs/llm-clients.md) | Anthropic vs OpenAI vs Claude Code, SSE parsing |
| [Tools & Skills](docs/tools-and-skills.md) | Tool trait, Skill trait, MCP skill, adding new tools |
| [Sandbox](docs/sandbox.md) | Allow/Deny/Redirect, OsSandbox, future designs |
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

500+ tests covering the full stack — SSE parsing, sandbox decisions, config loading, workspace persistence, and the agent loop with mock LLM clients.

## Inspired by

- **[OpenClaw](https://github.com/openclaw/openclaw)** — Multi-channel AI assistant. Dyson's controller architecture and workspace format come from OpenClaw.
- **[Hermes Agent](https://github.com/NousResearch/hermes-agent)** — Self-improving agent from [Nous Research](https://nousresearch.com/). Dyson's memory system is modeled after Hermes.
