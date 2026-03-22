# Dyson Documentation

Technical documentation for Dyson's internals.  Start with the architecture
overview, then dive into the component that interests you.

| Document | Covers |
|----------|--------|
| [Architecture Overview](architecture-overview.md) | The streaming loop, component wiring, data flow end-to-end |
| [Agent Loop](agent-loop.md) | The core loop: stream → detect tool calls → execute → repeat. Internal-tools providers (Claude Code) |
| [LLM Clients](llm-clients.md) | Anthropic, OpenAI, and Claude Code streaming. SSE parsing, thinking tokens, provider abstraction |
| [Tools & Skills](tools-and-skills.md) | Tool trait, Skill trait, BuiltinSkill, LocalSkill, adding new tools |
| [Sandbox](sandbox.md) | OS sandbox (Seatbelt/bubblewrap), Allow/Deny/Redirect, composition, MCP result sandboxing |
| [Memory](memory.md) | Tiered memory (always-in-context, FTS5 search, journals), nudges, character limits |
| [Chat Persistence](chat-persistence.md) | ChatHistory trait, per-chat agents, `/clear` and `/memory` commands |
| [Configuration](configuration.md) | dyson.json format, provider selection, skill config, env var resolution |
| [Secrets](secrets.md) | Per-secret scheme routing, InsecureEnvironmentVariable, zeroize, adding resolvers |
| [Tool Forwarding over MCP](tool-forwarding-over-mcp.md) | MCP server mode, bearer token auth, bidirectional MCP |
| [Adding a Provider](adding-a-provider.md) | How to add a new LLM provider (3-step process via the registry) |

**Key source files:**

```
src/
  main.rs                 CLI entry, subcommands (listen, init, run), systemd service install
  lib.rs                  Module tree, re-exports
  error.rs                DysonError (unified error type)
  message.rs              Message, Role, ContentBlock
  config/
    mod.rs                Settings, AgentSettings, LlmProvider, SkillConfig
    loader.rs             JSON config loader (dyson.json)
    hot_reload.rs         Config/workspace file watching
  tool/
    mod.rs                Tool trait, ToolContext, ToolOutput
    bash.rs               Shell execution with timeout
    workspace_view.rs     Read workspace files
    workspace_search.rs   Search workspace files by pattern
    workspace_update.rs   Write/append workspace files
    memory_search.rs      FTS5 memory search
  skill/
    mod.rs                Skill trait, create_skills() factory
    builtin.rs            BuiltinSkill (wraps built-in tools)
    local.rs              LocalSkill (SKILL.md parser, workspace discovery)
    mcp/
      mod.rs              McpSkill (client — connects to external MCP servers)
      serve.rs            McpHttpServer (server — exposes workspace tools with bearer auth)
      protocol.rs         Shared JSON-RPC types
      transport.rs        Stdio/HTTP MCP transports
  sandbox/
    mod.rs                Sandbox trait, SandboxDecision, create_sandbox()
    os.rs                 OsSandbox (macOS Seatbelt / Linux bubblewrap, output sanitization)
    composite.rs          CompositeSandbox (chain multiple sandboxes)
    no_sandbox.rs         DangerousNoSandbox (CLI-only bypass)
  secret/
    mod.rs                SecretResolver trait, SecretRegistry
    insecure_env.rs       InsecureEnvironmentVariable
  llm/
    mod.rs                LlmClient trait, CompletionConfig, create_client() factory
    registry.rs           Provider registry (aliases, defaults, env vars, factories)
    stream.rs             StreamEvent (TextDelta, ThinkingDelta, ToolUse*), StopReason
    anthropic.rs          Anthropic Messages API (extended thinking support)
    openai.rs             OpenAI Chat Completions API (reasoning_content support)
    openrouter.rs         OpenRouter (OpenAI-compatible wrapper with custom headers)
    claude_code.rs        Claude Code CLI subprocess (MCP server + bearer token)
    codex.rs              Codex CLI subprocess
  agent/
    mod.rs                Agent struct, the streaming loop
    stream_handler.rs     Consumes StreamEvents → Messages + ToolCalls (filters thinking)
  workspace/
    mod.rs                Workspace trait, skill_files() discovery
    openclaw.rs           OpenClawWorkspace (filesystem, skills/ auto-discovery)
    in_memory.rs          InMemoryWorkspace (for testing)
    memory_store.rs       SQLite FTS5 index for Tier 2 memory
    migrate.rs            Workspace versioning and migrations
  controller/
    mod.rs                Controller trait, Output trait, build_agent()
    terminal.rs           Terminal REPL controller
    telegram.rs           Telegram bot controller
  chat_history/
    mod.rs                ChatHistory trait
    disk.rs               Disk-backed JSON chat persistence
    in_memory.rs          In-memory chat store (for testing)
```
