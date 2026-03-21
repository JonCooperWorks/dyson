# Dyson Documentation

Technical documentation for Dyson's internals.  Start with the architecture
overview, then dive into the component that interests you.

| Document | Covers |
|----------|--------|
| [Architecture Overview](architecture-overview.md) | The streaming loop, component wiring, data flow end-to-end |
| [Agent Loop](agent-loop.md) | The core loop: stream → detect tool calls → execute → repeat. Internal-tools providers (Claude Code) |
| [LLM Clients](llm-clients.md) | Anthropic, OpenAI, and Claude Code streaming. SSE parsing, thinking tokens, provider abstraction |
| [Tools & Skills](tools-and-skills.md) | Tool trait, Skill trait, BuiltinSkill, adding new tools |
| [Sandbox](sandbox.md) | OS sandbox (Seatbelt/bubblewrap), Docker sandbox, Allow/Deny/Redirect, composition |
| [Memory](memory.md) | Tiered memory (always-in-context, FTS5 search, journals), nudges, character limits |
| [Chat Persistence](chat-persistence.md) | ChatStore trait, per-chat agents, `/clear` and `/memory` commands |
| [Configuration](configuration.md) | dyson.json format, provider selection, env var resolution |
| [Secrets](secrets.md) | Per-secret scheme routing, InsecureEnvironmentVariable, adding resolvers |

**Key source files:**

```
src/
  main.rs                 CLI entry, subcommands (listen, init, run), systemd service install
  lib.rs                  Module tree, re-exports
  error.rs                DysonError (unified error type)
  message.rs              Message, Role, ContentBlock
  config/
    mod.rs                Settings, AgentSettings, LlmProvider
    loader.rs             JSON config loader (dyson.json)
    hot_reload.rs         Config/workspace file watching
  tool/
    mod.rs                Tool trait, ToolContext, ToolOutput
    bash.rs               Shell execution with timeout
  skill/
    mod.rs                Skill trait (lifecycle hooks)
    builtin.rs            BuiltinSkill (wraps built-in tools)
    mcp.rs                MCP server integration (stdio + HTTP)
  sandbox/
    mod.rs                Sandbox trait, SandboxDecision, create_sandbox()
    os.rs                 OsSandbox (macOS Seatbelt / Linux bubblewrap)
    docker.rs             DockerSandbox (route bash to container)
    composite.rs          CompositeSandbox (chain multiple sandboxes)
    no_sandbox.rs         DangerousNoSandbox (CLI-only bypass)
  secret/
    mod.rs                SecretResolver trait, SecretRegistry
    insecure_env.rs       InsecureEnvironmentVariable
  llm/
    mod.rs                LlmClient trait, CompletionConfig, handles_tools_internally()
    stream.rs             StreamEvent (TextDelta, ThinkingDelta, ToolUse*), StopReason
    anthropic.rs          Anthropic Messages API (extended thinking support)
    openai.rs             OpenAI Chat Completions API (reasoning_content support)
    claude_code.rs        Claude Code CLI subprocess (no API key needed)
  agent/
    mod.rs                Agent struct, the streaming loop
    stream_handler.rs     Consumes StreamEvents → Messages + ToolCalls (filters thinking)
  controller/
    mod.rs                Controller trait, Output trait, build_agent()
    terminal.rs           Terminal REPL controller
    telegram.rs           Telegram bot controller
  persistence/
    mod.rs                Workspace (SOUL.md, MEMORY.md, journals)
```
