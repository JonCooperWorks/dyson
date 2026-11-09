# Configuration (`config`)

## Overview
Runtime configuration for Dyson. `mod.rs` defines `Settings` and every sub-type (agent, providers, skills, MCP servers, sandbox, Cedar, controllers, workspace, memory, compaction, chat history, transcriber, web search). `loader.rs` loads and validates `dyson.json` (internals in `loader/`), `migrate.rs` upgrades old config formats through a declarative step chain, and `hot_reload.rs` detects changes to the config and workspace files by mtime before each turn.

## Interface
- `Settings` and sub-types: `ActiveProvider`, `AgentSettings`, `ProviderConfig`, `LlmProvider { Anthropic, OpenAi, OpenRouter, ClaudeCode, Codex, OllamaCloud, Gemini, ... }`, `RateLimitConfig`, `SandboxConfig`, `CedarConfig`, `CedarMode`, `ControllerConfig`, `SkillConfig { Builtin | Local | Subagent | Mcp }`, `McpConfig`, `McpTransportConfig { Stdio | Http }`, `McpAuthConfig`, `LocalSkillConfig`, `BuiltinSkillConfig`, `SubagentSkillConfig`, `SubagentAgentConfig`, `WorkspaceConfig`, `MemoryConfig`, `CompactionConfig`, `ChatHistoryConfig`, `TranscriberConfig`, `WebSearchConfig` [mod.rs:Settings, mod.rs:LlmProvider, mod.rs:SkillConfig]
- `loader::load_settings(path: Option<&Path>) -> Result<Settings>`, `persist_model_selection`, `validate_subagent_configs` [loader.rs:load_settings]
- `migrate::{CURRENT_VERSION = 3, migrate(root: &mut Value) -> Result<bool>}` [migrate.rs:CURRENT_VERSION, migrate.rs:migrate]
- `HotReloader::new(config_path, workspace_path)`, `check() -> Result<(bool, Option<Settings>)>` [hot_reload.rs:HotReloader]
- [loader](./loader/EGM.md) - discovery, schema, parsing, secret resolution, validation

## Invariants
- Load order: explicit path (error if missing), `./dyson.json`, `~/.config/dyson/dyson.json`, else built-in defaults; files are migrated before parsing [loader.rs:load_settings]
- Migrations are composed of declarative steps (move, copy, set, remove, bail) up to `CURRENT_VERSION` [migrate.rs:migrate]
- API keys are held as `crate::auth::Credential` (zeroized on drop) [mod.rs:AgentSettings, mod.rs:ProviderConfig]
- Hot reload compares mtimes, debounced by `DEBOUNCE_DURATION = 500 ms` [hot_reload.rs:DEBOUNCE_DURATION]

## Side Effects
- Reads config and workspace file metadata; loader may write migrated config back with `0o600` [hot_reload.rs:HotReloader::check, loader.rs:load_settings]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
