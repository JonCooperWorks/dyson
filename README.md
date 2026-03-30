# Dyson

A streaming AI agent loop in Rust, built to understand how these things actually work — and how to secure them.

> **This is an educational project.** I'm an AppSec engineer building this to demystify AI agents — how they work, how to secure them, and how to deploy them responsibly. If you need a production agent, see [the projects that inspired this](#inspired-by). If you want to understand what's happening inside the loop, read on.

## Why

AI agents run tools in a loop. They read your files, execute commands, make network requests. If you're going to trust one with access to your system, you should understand exactly what's happening inside that loop.

As someone who works in application security, I found myself reviewing agent deployments without a clear mental model of what was actually going on under the hood. The best way to understand something is to build it from scratch — so that's what Dyson is. A from-scratch implementation of the core agent pattern: **LLM streams a response → detects tool calls → executes them → feeds results back → repeats**. Every component is a trait, every decision point is a hook, and every file is heavily annotated explaining *why* it works the way it does.

The goal isn't to replace Claude Code or Cursor — it's to demystify how agent loops work, where the security boundaries are, and what controls are possible.

## Inspired by

Dyson is heavily inspired by two projects that are worth checking out if you want something production-ready:

- **[OpenClaw](https://github.com/openclaw/openclaw)** — A personal AI assistant that runs on your own hardware with support for dozens of channels (Telegram, Slack, Discord, WhatsApp, Signal, etc.), browser control, a skill registry, and voice. Dyson's multi-controller architecture and workspace file format come directly from how OpenClaw handles channels and agent identity.

- **[Hermes Agent](https://github.com/NousResearch/hermes-agent)** — An agent framework from [Nous Research](https://nousresearch.com/) with a well-thought-out memory system: agent-curated journals, persistent identity files, periodic nudges, and full-text search. Dyson's memory architecture is modeled after Hermes, and the workspace format is compatible with the OpenClaw format that Hermes uses.

If you want a production-ready agent, start with one of those. If you want to understand what's happening inside the loop and how to secure it, that's what Dyson is for.

## What you learn by reading this codebase

- **How streaming works** — SSE parsing, partial JSON accumulation, token-by-token delivery
- **How tool calling works** — the LLM emits structured `tool_use` blocks, the agent executes them, results go back as `tool_result` messages
- **Where the security boundaries are** — every tool call passes through a `Sandbox` trait that can allow, deny, or redirect. This is where you'd enforce policies
- **How MCP fits in** — MCP servers are just another skill implementation. The agent loop doesn't know they exist
- **How providers differ** — Anthropic and OpenAI have different SSE formats, different message schemas, different tool calling conventions. The `LlmClient` trait abstracts all of it

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
| `LlmClient` | Stream completions from any provider (Anthropic, OpenAI, OpenRouter, Claude Code, Codex) |
| `Tool` | A single callable capability (bash, web search, MCP remote tool) |
| `Skill` | A bundle of tools with lifecycle hooks and prompt fragments |
| `Sandbox` | Gate every tool call — allow, deny, redirect, audit |
| `Controller` | Own the input/output lifecycle (terminal REPL, Telegram bot) |
| `SecretResolver` | Resolve secrets from any backend (env vars, Vault, SSM) |

## Providers

Five LLM backends, selectable via `--provider` or config:

| Provider | Config value | API key | Notes |
|----------|-------------|---------|-------|
| Anthropic | `"anthropic"` | `ANTHROPIC_API_KEY` | Default. Full streaming + structured tool calling |
| OpenAI | `"openai"` | `OPENAI_API_KEY` | Also works with Ollama, vLLM, Together, rLLM, etc. via `base_url` |
| OpenRouter | `"openrouter"` | `OPENROUTER_API_KEY` | 200+ models via OpenAI-compatible API |
| Claude Code | `"claude-code"` | None (uses stored creds) | Spawns the `claude` CLI. Zero config. Claude Code handles tools |
| Codex | `"codex"` | None (uses stored creds) | Spawns the `codex` CLI. OpenAI Codex agent loop |

Adding a new provider is a 3-step process — see [Adding a Provider](docs/adding-a-provider.md).

## Quick start

```bash
# With Anthropic API key
export ANTHROPIC_API_KEY="sk-ant-..."
cargo run -- --dangerous-no-sandbox "what files are in this directory?"

# Interactive mode
cargo run -- --dangerous-no-sandbox

# With Claude Code (no API key needed)
cargo run -- --dangerous-no-sandbox --provider claude-code "hello"

# With a config file
cargo run -- --dangerous-no-sandbox --config examples/claude-code-telegram.json

# With a local model (Ollama, vLLM, rLLM, etc.)
cargo run -- --dangerous-no-sandbox --provider openai --base-url http://localhost:9000 --model llama-3.2-3b-instruct "hello"
```

The `--dangerous-no-sandbox` flag is required — it's an explicit acknowledgment that tool calls are unrestricted.

## Configuration

Dyson uses JSON config files (`dyson.json`). See the `examples/` folder for ready-to-use configs:

| Example | What it does |
|---------|-------------|
| [`examples/claude-code-telegram.json`](examples/claude-code-telegram.json) | Claude Code as LLM, Telegram bot, Context7 MCP server |
| [`examples/local-llm-telegram.json`](examples/local-llm-telegram.json) | Local model (rLLM/Ollama) as LLM, Telegram bot |

### Minimal config

```json
{
  "agent": {
    "provider": "anthropic",
    "model": "claude-sonnet-4-20250514"
  }
}
```

### Web search

Give models access to web search by adding a `web_search` section. Supports
Brave Search (API key required) and SearXNG (free public instances, no key):

```json
{
  "web_search": {
    "provider": "searxng",
    "base_url": "https://searx.be"
  }
}
```

Or with Brave Search:

```json
{
  "web_search": {
    "provider": "brave",
    "api_key": { "resolver": "insecure_env", "name": "BRAVE_API_KEY" }
  }
}
```

The `web_search` tool only appears when configured — otherwise models don't see it.
The search backend is a trait (`SearchProvider`), so adding new providers is straightforward.

### Full config with MCP, Telegram, web search, and secrets

```json
{
  "agent": {
    "provider": "claude-code",
    "model": "sonnet"
  },
  "web_search": {
    "provider": "searxng",
    "base_url": "https://searx.be"
  },
  "mcp_servers": {
    "context7": {
      "url": "https://mcp.context7.com/mcp",
      "headers": {
        "CONTEXT7_API_KEY": {
          "resolver": "insecure_env",
          "name": "CONTEXT7_API_KEY"
        }
      }
    }
  },
  "controllers": [
    {
      "type": "telegram",
      "bot_token": { "resolver": "insecure_env", "name": "TELEGRAM_API_KEY" },
      "allowed_chat_ids": [123456789]
    }
  ],
  "skills": {
    "builtin": {
      "tools": ["bash"]
    }
  }
}
```

### Secrets

Every secret can be a literal string or a resolver reference:

```json
"bot_token": "literal-token-here"
"bot_token": { "resolver": "insecure_env", "name": "TELEGRAM_API_KEY" }
```

The `insecure_env` resolver reads from environment variables. Future resolvers: Vault, AWS SSM, 1Password CLI.

## Project structure

```
src/
  main.rs              CLI entry point
  lib.rs               Module tree
  error.rs             DysonError — unified error type
  message.rs           Message, Role, ContentBlock
  config/              Settings, JSON loader
  tool/                Tool trait + bash, web search, workspace tools
  skill/               Skill trait, BuiltinSkill, MCP skill
  sandbox/             Sandbox trait, OsSandbox, DangerousNoSandbox
  secret/              SecretResolver trait, InsecureEnvironmentVariable
  llm/                 LlmClient trait + provider registry + Anthropic, OpenAI, OpenRouter, Claude Code, Codex
  agent/               The loop + stream handler
  controller/          Controller trait + terminal, Telegram
examples/              Example dyson.json configs
docs/                  Component documentation
```

Every file has extensive annotations in the style of [rLLM](https://github.com/joncooperworks/rllm) — learning overviews, architecture diagrams, design decision explanations, and inline commentary on non-obvious code.

## Docs

| Document | Covers |
|----------|--------|
| [Architecture Overview](docs/architecture-overview.md) | End-to-end data flow, component hierarchy, core traits |
| [Agent Loop](docs/agent-loop.md) | The streaming loop, tool execution, error recovery |
| [LLM Clients](docs/llm-clients.md) | Anthropic vs OpenAI vs Claude Code, SSE parsing |
| [Tools & Skills](docs/tools-and-skills.md) | Tool trait, Skill trait, MCP skill, adding new tools |
| [Sandbox](docs/sandbox.md) | Allow/Deny/Redirect, OsSandbox, future designs |
| [Secrets](docs/secrets.md) | Per-secret resolver routing, InsecureEnvironmentVariable |
| [Configuration](docs/configuration.md) | dyson.json format, provider selection |
| [Adding a Provider](docs/adding-a-provider.md) | 3-step process to add a new LLM provider |
| [Memory](docs/memory.md) | Tiered memory system, FTS5 search, journals |
| [Chat Persistence](docs/chat-persistence.md) | ChatHistory trait, per-chat agents |
| [Tool Forwarding over MCP](docs/tool-forwarding-over-mcp.md) | MCP server mode, bearer token auth |

## Tests

```bash
cargo test
```

339 tests covering SSE parsing, message serialization, config loading, bash execution, stream handling, sandbox decisions, secret resolution, provider registry, workspace persistence, web search, and the agent loop with mock LLM clients.
