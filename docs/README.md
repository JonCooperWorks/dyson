# Dyson Documentation

Technical documentation for Dyson's internals.  Start with the architecture
overview for the big picture, then follow the data flow outward: the agent
loop and LLM clients handle streaming, tools and skills handle side effects,
the sandbox polices them, memory and the knowledge base give the agent long-
term state, and controllers plug it into channels like the terminal, Telegram,
and the web UI.  Everything after that — subagents, advisor, dreaming —
layers on top of those primitives.

| Document | Covers |
|----------|--------|
| [Architecture Overview](architecture-overview.md) | The streaming loop, component wiring, data flow end-to-end |
| [Agent Loop](agent-loop.md) | The core loop: stream → detect tool calls → execute → repeat. Internal-tools providers (Claude Code) |
| [LLM Clients](llm-clients.md) | Anthropic, OpenAI, and Claude Code streaming. SSE parsing, thinking tokens, provider abstraction |
| [Tools & Skills](tools-and-skills.md) | Tool trait, Skill trait, BuiltinSkill, LocalSkill, adding new tools |
| [Tool Execution Pipeline](tool-execution-pipeline.md) | Rate limiting, dependency analysis, result formatting, lifecycle hooks |
| [AST-Aware Code Editing & Reading](ast.md) | Shared tree-sitter infra in `tool::ast`: `bulk_edit` rename/find_replace/list_definitions, `read_file` symbol extraction, `search_files` identifier mode, across 19 languages |
| [Sandbox](sandbox.md) | OS sandbox (Seatbelt/bubblewrap), Allow/Deny/Redirect, composition, MCP result sandboxing |
| [Memory](memory.md) | Tiered memory (always-in-context, FTS5 search, journals), nudges, character limits |
| [Knowledge Base](knowledge-base.md) | Document storage + FTS5 search: kb/raw (source material), kb/wiki (articles), INDEX.md (system prompt index) |
| [Chat Persistence](chat-persistence.md) | ChatHistory trait, per-chat agents, `/clear` and `/memory` commands |
| [Configuration](configuration.md) | dyson.json format, provider selection, skill config, env var resolution |
| [Secrets](secrets.md) | Per-secret scheme routing, InsecureEnvironmentVariable, zeroize, adding resolvers |
| [Tool Forwarding over MCP](tool-forwarding-over-mcp.md) | MCP server mode, bearer token auth, bidirectional MCP |
| [Web UI / HTTP Controller](web.md) | HttpController, reactive frontend store, SSE turn lifecycle, auth, persistence |
| [Subagents](subagents.md) | Child agents, orchestrators (security_engineer), OrchestratorConfig composability, parallel dispatch |
| [Security Engineer Subagent](security-engineer-subagent.md) | Flagship subagent: AST-aware vuln hunting, prompt techniques against weak models, model tuning loop |
| [Advisor](advisor.md) | Consult a stronger model; native Anthropic `advisor_20260301` or generic subagent fallback |
| [Public Agents](public-agents.md) | Group chat agents, AgentMode enum, tool restriction, SSRF protection, Telegram privacy mode |
| [Adding a Provider](adding-a-provider.md) | How to add a new LLM provider (3-step process via the registry) |
| [Prompt Caching](prompt-caching.md) | Why the prompt is split into stable/ephemeral parts, the 4-breakpoint Anthropic strategy, how KV prefix caching works |
| [MCP OAuth](mcp-oauth.md) | OAuth 2.0 Authorization Code + PKCE for MCP servers |
| [Dreaming](dreaming.md) | Background cognitive tasks (memory maintenance, learning synthesis, self-improvement) |
| [Testing & Tuning](testing.md) | Four test layers (unit / smoke / regression / live subagent review), smoke-to-regression promotion, run-grade-tune-run loop, case study tuning `security_engineer.md` against Qwen |
| [security_engineer Tuning: qwen3.6-plus](tuning-qwen36-plus-summary.md) | 7-iteration tuning summary against `qwen3.6-plus` — what each prompt change did, evidence trail |
| [Comparison: Hermes Agent](comparison-hermes-agent.md) | Side-by-side with Hermes Agent (Nous Research) |

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
    migrate.rs            Config versioning and migrations (v0 → v1 → v2)
    hot_reload.rs         Config/workspace file watching
  tool/
    mod.rs                Tool trait, ToolContext, ToolOutput
    bash.rs               Shell execution with timeout
    read_file.rs          Read workspace files (line range, or AST symbol extraction)
    write_file.rs         Create or overwrite files
    edit_file.rs          Pattern-based find-and-replace editing
    list_files.rs         Glob-based file discovery
    search_files.rs       Content search (regex, or AST identifier mode)
    bulk_edit/            Multi-file edit: AST rename_symbol, find_replace, list_definitions (tree-sitter, 19 languages)
    ast/                  Shared tree-sitter grammars and walkers used by bulk_edit, read_file, search_files
    security/             AST-aware security tools (ast_query, attack_surface_analyzer, exploit_builder)
    workspace.rs          Unified view/list/search/update for workspace files
    memory_search.rs      FTS5 memory search
    kb_search.rs          FTS5 knowledge base search (scope: all/raw/wiki)
    kb_status.rs          Knowledge base statistics and file listing
    web_search.rs         Web search (Brave, SearXNG)
    load_skill.rs         On-demand skill loading
    skill_create.rs       Create/update/improve skills
    send_file.rs          Send file to user via controller
    export_conversation.rs  Export chat history (ShareGPT format)
  skill/
    mod.rs                Skill trait, create_skills() factory
    builtin.rs            BuiltinSkill (wraps built-in tools)
    local.rs              LocalSkill (SKILL.md parser, workspace discovery)
    subagent/             SubagentSkill, OrchestratorTool, CoderTool, spawn_child
      orchestrator.rs     Generic composable orchestrator (OrchestratorConfig + OrchestratorTool)
      security_engineer.rs  Security engineer config (composed from OrchestratorTool)
      coder.rs            Directory-scoped coding subagent
      prompts/            System prompts and protocol injections for all subagents
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
    mod.rs                LlmClient trait, CompletionConfig, create_client()
    registry.rs           Provider registry (aliases, defaults, env vars, factories)
    stream.rs             StreamEvent (TextDelta, ThinkingDelta, ToolUse*), StopReason
    anthropic.rs          Anthropic Messages API (extended thinking support)
    openai.rs             OpenAI Chat Completions API (reasoning_content support)
    openrouter.rs         OpenRouter (OpenAI-compatible wrapper with custom headers)
    claude_code.rs        Claude Code CLI subprocess (MCP server + bearer token)
    codex.rs              Codex CLI subprocess
  tool_hooks.rs           Pre/post tool execution lifecycle hooks
  agent/
    mod.rs                Agent struct, the streaming loop
    stream_handler.rs     Consumes StreamEvents → Messages + ToolCalls (filters thinking)
    compaction.rs         Five-phase context window summarization (Hermes-style)
    dependency_analyzer.rs  Dependency-aware tool call grouping (parallel vs sequential)
    result_formatter.rs   Structured, LLM-optimized tool output formatting
    token_budget.rs       Cumulative token usage tracking
    tool_limiter.rs       Per-turn rate limiting and cooldown enforcement
    rate_limiter.rs       Per-agent message rate limiting
    reflection.rs         Agent state introspection
    dream.rs              Background cognitive tasks (DreamRunner, DreamHandle)
    reflection.rs         Built-in dreams (learning synthesis, memory maintenance, self-improvement)
    silent_output.rs      Null output sink for internal LLM calls (compaction, learning)
  workspace/
    mod.rs                Workspace trait, skill_files() discovery
    filesystem.rs           FilesystemWorkspace (filesystem, skills/ auto-discovery)
    in_memory.rs          InMemoryWorkspace (for testing)
    memory_store.rs       SQLite FTS5 index for Tier 2 memory
    migrate.rs            Workspace versioning and migrations
  controller/
    mod.rs                Controller trait, Output trait, build_agent(), ActivityRegistry, BrowserArtefactSink
    terminal.rs           Terminal REPL controller
    telegram/             Telegram bot controller (chat dispatch, feedback emoji, media)
    http/                 HTTP + SSE controller, embedded React UI
      mod.rs              HttpController, HttpState, ChatHandle, SseOutput, route dispatch
      assets.rs           Compile-time asset table generated by build.rs
      web/                Vite + React frontend (npm-built, embedded by build.rs)
    background.rs         Background-loop controller
    activity.rs           ActivityRegistry shared across controllers
    recording.rs          Transcript recording for replay
  auth/                   Auth trait, BearerTokenAuth, DangerousNoAuth (HTTP + MCP)
  chat_history/
    mod.rs                ChatHistory trait
    disk.rs               Disk-backed JSON chat persistence
    in_memory.rs          In-memory chat store (for testing)
```
