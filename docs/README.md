# Dyson Documentation

Technical documentation for Dyson, the Rust agent runtime. Read the overview
first, then follow the request path outward: controllers receive input, the
agent loop streams model output, tools and skills provide capabilities, the
sandbox gates side effects, and persistence keeps chats/workspace state usable
across turns.

## Index

| Document | Covers |
|---|---|
| [Architecture Overview](architecture-overview.md) | Runtime shape, component wiring, request flow, core traits |
| [Agent Loop](agent-loop.md) | Stream, tool detection, dependency grouping, execution, error recovery |
| [LLM Clients](llm-clients.md) | Provider registry, SSE parsing, API vs CLI-subprocess providers |
| [Tools & Skills](tools-and-skills.md) | `Tool`, `Skill`, built-ins, local skills, MCP skills, adding tools |
| [Tool Execution Pipeline](tool-execution-pipeline.md) | Rate limits, dependency analysis, formatting, hooks |
| [AST-Aware Code Editing & Reading](ast.md) | Tree-sitter-backed reads, searches, and multi-file edits |
| [Sandbox](sandbox.md) | `PolicySandbox`, OS wrappers, app-level path/network checks |
| [Configuration](configuration.md) | `dyson.json`, schema migrations, providers, controllers, skills |
| [Secrets](secrets.md) | Secret resolver routing and env-backed credentials |
| [LLM Prompt Caching](prompt-caching.md) | Stable/ephemeral prompt segments and Anthropic cache breakpoints |
| [Tool Forwarding over MCP](tool-forwarding-over-mcp.md) | Dyson as an MCP server for CLI-subprocess providers |
| [MCP OAuth](mcp-oauth.md) | OAuth 2.0 Authorization Code + PKCE for MCP servers |
| [Elicitation](elicitation.md) | Server-initiated prompts in bidirectional MCP: broker, HTTP bridge, UI form |
| [Web UI / HTTP Controller](web.md) | Embedded frontend, JSON/SSE APIs, auth, persistence |
| [Chat Persistence](chat-persistence.md) | `ChatHistory`, per-chat agents, clear/memory commands |
| [Memory](memory.md) | Tiered memory, journals, nudges, FTS5 search |
| [Knowledge Base](knowledge-base.md) | Raw/wiki knowledge-base files and indexes |
| [Subagents](subagents.md) | Child agents, tool inheritance, delegation |
| [Advisor](advisor.md) | Stronger-model consultation path |
| [Dreaming](dreaming.md) | Background memory and skill-learning tasks |
| [Public Agents](public-agents.md) | Group chat agents and restricted public-channel tools |
| [Security Engineer Subagent](security-engineer-subagent.md) | Vulnerability-review subagent design |
| [Testing & Tuning](testing.md) | Test layers, smoke-to-regression workflow, prompt tuning |
| [Skill Marketplaces](skill-marketplaces.md) | Swarm-hosted skill marketplace draft |
| [Adding a Provider](adding-a-provider.md) | Provider-registry implementation steps |
| [Hermes Comparison](comparison-hermes-agent.md) | Comparison with Hermes Agent |

## Source Map

```text
crates/dyson/src/
  main.rs                 CLI entry: listen, init, hash-bearer, swarm, run
  command/                Subcommand implementations and config overrides
  config/                 Settings, loader, migrations, hot reload
  llm/                    Provider registry, API clients, CLI-subprocess clients, SSE parsers
  agent/                  Agent loop, stream handler, compaction, rate limits, dependency analysis
  tool/                   Built-in tools, web/search/file/workspace tools
  ast/                    Tree-sitter language registry and AST query helpers
  skill/                  Builtin, local, MCP, subagent, marketplace-loaded skills
  media/                  Attachments, generated artefacts, PDF/image/audio handling
  sandbox/                Policy sandbox, OS command builders, no-sandbox bypass
  controller/             Terminal, HTTP/web, Telegram, background controllers
  chat_history/           Disk chat store and flat-layout migrations
  workspace/              Filesystem/in-memory workspace, memory store, migrations
  auth/                   Bearer, hashed bearer, OIDC/no-auth shared auth traits
  secret/                 Secret resolver registry
  swarm_state_sync.rs     Swarm-mode state mirror worker
```

When a behaviour changes, update the doc closest to that code seam rather than
adding a broad note somewhere else.
