# Dyson Documentation

Technical documentation for Dyson's internals.  Start with the architecture
overview, then dive into the component that interests you.

| Document | Covers |
|----------|--------|
| [Architecture Overview](architecture-overview.md) | The streaming loop, component wiring, data flow end-to-end |
| [Agent Loop](agent-loop.md) | The core loop: stream → detect tool calls → execute → repeat |
| [LLM Clients](llm-clients.md) | Anthropic and OpenAI streaming, SSE parsing, provider abstraction |
| [Tools & Skills](tools-and-skills.md) | Tool trait, Skill trait, BuiltinSkill, adding new tools |
| [Sandbox](sandbox.md) | The Sandbox trait, Allow/Deny/Redirect, DangerousNoSandbox |
| [Configuration](configuration.md) | dyson.toml format, provider selection, env var resolution |
| [Secrets](secrets.md) | Per-secret scheme routing, InsecureEnvironmentVariable, adding resolvers |

**Key source files:**

```
src/
  main.rs                 CLI entry, interactive REPL
  lib.rs                  Module tree, re-exports
  error.rs                DysonError (unified error type)
  message.rs              Message, Role, ContentBlock
  config/
    mod.rs                Settings, AgentSettings, LlmProvider
    dyson_toml.rs         TOML config loader
  tool/
    mod.rs                Tool trait, ToolContext, ToolOutput
    bash.rs               Shell execution with timeout
  skill/
    mod.rs                Skill trait (lifecycle hooks)
    builtin.rs            BuiltinSkill (wraps built-in tools)
  sandbox/
    mod.rs                Sandbox trait, SandboxDecision
    no_sandbox.rs         DangerousNoSandbox (passthrough)
  secret/
    mod.rs                SecretResolver trait, SecretRegistry
    insecure_env.rs       InsecureEnvironmentVariable
  llm/
    mod.rs                LlmClient trait, CompletionConfig
    stream.rs             StreamEvent, StopReason
    anthropic.rs          Anthropic Messages API
    openai.rs             OpenAI Chat Completions API
  agent/
    mod.rs                Agent struct, the streaming loop
    stream_handler.rs     Consumes StreamEvents → Messages + ToolCalls
  ui/
    mod.rs                Output trait
    terminal.rs           Terminal renderer (streaming text)
```
