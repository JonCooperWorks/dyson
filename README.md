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
- **Provider differences** — Anthropic, OpenAI, and Gemini have different SSE formats, schemas, and tool conventions
- **AST tooling** — statically-linked tree-sitter grammars, identifier-node rewriting, graceful word-boundary text fallback

## What it does

Dyson is a fully functional agent — not just a learning exercise.  It can hold its own on real engineering work.

- **Dyson can code** — `bulk_edit` parses 19 languages with tree-sitter before it touches anything, so renaming `Config` doesn't also rewrite `ConfigManager`, strings, or comments.  Files outside the grammar set (Markdown, YAML, Dockerfiles, shell) fall back to a word-boundary text replace, so a single rename sweeps source, docs, and config in one pass.  The same AST infrastructure powers symbol-aware reads (`read_file` with `symbol: "..."` extracts one definition out of a large file) and identifier-only searches (`search_files` with `ast: true` audits where a symbol is used without false positives in strings or comments).  See [AST-Aware Code Editing & Reading](docs/ast.md).
- **Self-improvement** — background "dream" tasks distill completed work into `SKILL.md` files that hot-reload via mtime watching
- **Sandboxed tool execution** — every tool call passes through a `Sandbox` that can allow, deny, or redirect; the default `PolicySandbox` wraps bash in OS-level sandboxes (macOS Apple Containers / Linux bubblewrap)
- **Workspace persistence** — conversations, memory, and a knowledge base in SQLite with tiered memory and FTS5 search
- **Context compaction** — five-phase summarization when the context window fills; pre-compaction history rotated for fine-tuning
- **Subagents** — child agents with different models and tool sets, inheriting parent tools
- **Multi-channel** — terminal REPL and Telegram bot concurrently; add channels via the `Controller` trait
- **Dependency analysis** — tool calls grouped by resource dependencies, executed in parallel when safe
- **Image generation** — pluggable image generation via the `image_generate` tool; reuses existing providers
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

## Web UI

A built-in HTTP controller serves a local web UI plus a JSON + SSE API
so you can talk to your agent in a browser instead of the terminal.
Add it to `dyson.json`:

```json
{ "type": "http", "bind": "127.0.0.1:7878" }
```

Open `http://127.0.0.1:7878/`.  Same `ChatHistory` and `FeedbackStore`
as the Telegram controller — chats and ratings sync both ways.

> ## ⚠️ DO NOT EXPOSE THE WEB UI TO THE PUBLIC INTERNET
>
> **The HTTP controller has no inbound authentication.**  Anyone who
> can reach the bind address can talk to your agent, edit your
> workspace files (`SOUL.md`, `MEMORY.md`, journal entries), switch
> your model, read every chat in `~/.dyson/chats`, and burn your
> provider credits.  The default `127.0.0.1` bind is the **only**
> supported deployment.
>
> **For remote access, tunnel — never expose:**
> - **SSH:** `ssh -L 7878:127.0.0.1:7878 user@your-host` then open
>   `http://127.0.0.1:7878/` on your laptop.
> - **Tailscale:** bind to `127.0.0.1:7878`, install Tailscale on
>   server + client, reach the loopback over the mesh — Tailscale
>   ACLs gate who can connect.
> - **Cloudflare Tunnel / WireGuard:** same pattern.
>
> Do **not** bind to `0.0.0.0` on a public host.  Do **not** put a
> reverse proxy in front of it without your own auth in the proxy.
> The web UI assumes a single trusted operator behind loopback.

See [docs/web.md](docs/web.md) for the full API, SSE event schema,
typed `ToolView` payloads, persistence story, and known limits.

## Swarm (trusted-network only)

The swarm distributes tasks across Dyson nodes. A central hub signs work with Ed25519; nodes verify before executing.

**Security model:** Ed25519 signing ensures tasks came from the hub you trust. It does **not** authenticate or authorize callers. `/mcp` and `/swarm/register` are open endpoints — anyone who can reach the hub can register as a node or dispatch tasks.

**Recommended transports:** run the hub on a Tailscale mesh or behind an SSH port-forward. TLS is mandatory for off-localhost binds but it encrypts traffic — it does not gate callers. If you deploy the hub on the public internet, firewall it to known peers.

See [docs/swarm.md](docs/swarm.md) for the full architecture, configuration, and task lifecycle.


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

Seven LLM backends, selectable via `--provider` or config:

| Provider | Config value | API key | Notes |
|----------|-------------|---------|-------|
| Anthropic | `"anthropic"` | `ANTHROPIC_API_KEY` | Default. Full streaming + structured tool calling |
| Gemini | `"gemini"` | `GEMINI_API_KEY` | Google Gemini. Chat + image generation (Nano Banana 2) |
| OpenAI | `"openai"` | `OPENAI_API_KEY` | Also works with vLLM, Together, rLLM, etc. via `base_url` |
| OpenRouter | `"openrouter"` | `OPENROUTER_API_KEY` | 200+ models via OpenAI-compatible API |
| Ollama Cloud | `"ollama-cloud"` | `OLLAMA_API_KEY` | Cloud-hosted models on ollama.com |
| Claude Code | `"claude-code"` | None (uses stored creds) | Spawns the `claude` CLI. Zero config |
| Codex | `"codex"` | None (uses stored creds) | Spawns the `codex` CLI. OpenAI Codex agent loop |

Adding a new provider is a 3-step process — see [Adding a Provider](docs/adding-a-provider.md).

## Image generation

The `image_generate` tool lets the agent create images from text descriptions. Generated images are delivered to the user as files via the controller (printed as paths in terminal, sent as documents in Telegram).

Configure it by pointing `image_generation_provider` at any provider that supports image generation:

```json
{
  "providers": {
    "gemini": {
      "type": "gemini",
      "api_key": { "resolver": "insecure_env", "name": "GEMINI_API_KEY" }
    }
  },
  "agent": {
    "provider": "anthropic",
    "image_generation_provider": "gemini",
    "image_generation_model": "gemini-3-pro-image-preview"
  }
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `image_generation_provider` | Yes | Name of a provider from the `"providers"` map |
| `image_generation_model` | No | Model override (defaults to the provider's default model) |

When `image_generation_provider` is not set, the `image_generate` tool is simply absent — no errors. The provider must support image generation (currently: Gemini via the `gemini-3-pro-image-preview` model).

A single Gemini provider can serve both chat and image generation:

```json
{
  "providers": {
    "gemini": { "type": "gemini", "api_key": "..." }
  },
  "agent": {
    "provider": "gemini",
    "image_generation_provider": "gemini",
    "image_generation_model": "gemini-3-pro-image-preview"
  }
}
```

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
| [AST-Aware Code Editing & Reading](docs/ast.md) | Shared tree-sitter infra: `bulk_edit` rename/find_replace/list_definitions, `read_file` symbol extraction, `search_files` identifier mode |
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
| [Testing & Tuning](docs/testing.md) | Four test layers, smoke-to-regression promotion, live subagent review, case study tuning a prompt against Qwen |

## Tests

```bash
cargo test
```

1100+ tests covering the full stack — SSE parsing, sandbox decisions, config loading, workspace persistence, and the agent loop with mock LLM clients.

### Smoke tests against real repos

Three `smoke_*` examples stress the AST tools against shallow clones of popular open-source projects.  They don't hit an LLM — they're just deterministic exercises of the parser, index, and query/describe/trace paths against production-scale code.  Failures here become regression tests under `tests/ast_taint_patterns.rs`.

```bash
cargo run -p dyson --example smoke_ast_query --release
cargo run -p dyson --example smoke_ast_describe --release
cargo run -p dyson --example smoke_taint_trace --release
```

### Tuning the security prompt against your model (billable)

The `security_engineer.md` system prompt was tuned against Claude.  If you run Dyson against a different model (a smaller local model, a cheap OpenRouter model, etc.), its failure modes will differ — it may skip `taint_trace`, hallucinate paths, or produce shallower reports.  [`examples/expensive_live_security_review.rs`](crates/dyson/examples/expensive_live_security_review.rs) spins up the **real** orchestrator (direct tools + inner planner/researcher/coder/verifier) against a fixed set of deliberately-vulnerable repos (Juice Shop, NodeGoat, RailsGoat) using **whatever provider and model `dyson.json` resolves to**, so you can grade the resulting reports and feed the gaps back into the prompt.

```bash
# Single target, default model from dyson.json.
cargo run -p dyson --example expensive_live_security_review --release -- \
    --config dyson.json --target juice-shop

# Full sweep (fans out across every target — hence the long flag name).
cargo run -p dyson --example expensive_live_security_review --release -- \
    --config dyson.json --expensive-scan-all-targets
```

This is **not** a `cargo test`.  It makes real, billable LLM calls.  Reports land in `/tmp/dyson-security-review-<name>.md`.  See [Subagents → Tuning against your production model](docs/subagents.md#tuning-against-your-production-model) for how to evaluate the output.

## Running in production

Dyson uses [jemalloc](https://jemalloc.net) as its global allocator on non-MSVC targets (via `tikv-jemallocator`).  Jemalloc's defaults retain freed memory for up to 10 seconds before returning it to the OS, which can make `RSS` look inflated for long-lived agents.  If your deployment has tight memory budgets (cgroups, systemd `MemoryHigh`, containers), set:

```bash
export MALLOC_CONF="dirty_decay_ms:1000,muzzy_decay_ms:1000"
```

This returns freed pages to the OS within ~1 second at the cost of a little extra `madvise()` churn.  Acceptable for most workloads; leave defaults if throughput is the priority.

## Inspired by

- **[Hermes Agent](https://github.com/NousResearch/hermes-agent)** — Self-improving agent from [Nous Research](https://nousresearch.com/). Dyson's memory system is modeled after Hermes.
- **[OpenClaw](https://github.com/openclaw/openclaw)** — Multi-channel AI assistant. Dyson's controller architecture and workspace format come from OpenClaw.
- **[zeroclaw](https://github.com/zeroclaw-labs/zeroclaw)** — Minimal agent focused on resource consumption.
