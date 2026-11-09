# Controllers (`controller`)

## Overview
Interaction channels and the contracts they share. A `Controller` owns an input loop and an `Output` renders agent events. Shared machinery here covers rate-limited LLM clients (`ClientRegistry`), agent construction by trust level (`build_agent`, `AgentMode`), runtime config reload and a settings bus, built-in and executable slash commands, background agent runs, an activity registry, and log tails. Transport adapters live in `http/`, `telegram/`, and `terminal.rs`.

## Interface
- `Controller` trait (`name`, `run`, `system_prompt`) [mod.rs:Controller]; `Output` trait [output.rs:Output]
- `AgentMode { Private | Public }` [mod.rs:AgentMode]; `build_agent(...)` [agent_builder.rs:build_agent]
- `ClientRegistry` [client_registry.rs:ClientRegistry]
- `active_provider_name`, `list_providers`, `WARMUP_PLACEHOLDER = "warmup-placeholder"` [mod.rs:active_provider_name, mod.rs:WARMUP_PLACEHOLDER]
- Runtime: `create_hot_reloader`, `check_and_reload_agent`, `ReloadOutcome`, `install_settings_bus`, `publish_settings`, `subscribe_settings_updates`, `install_explicit_config_path`, `resolve_config_path_for_runtime`, `BrowserArtefactSink`, `install_browser_artefact_sink`, `browser_artefact_sink` [runtime.rs:check_and_reload_agent]
- Commands: `CommandResult`, `ProviderInfo`, `ModelInfo`, `ModelState`, `BackgroundCompletion`, `execute_lockfree_command`, `execute_agent_command` [commands.rs:execute_agent_command]
- Slash commands: `SlashCommandInfo`, `ExecutableSlashCommand`, `SlashDispatch`, `builtin_commands`, `commands_for_settings`, `find_executable`, `parse_command`, `should_handle_without_llm`, `dispatch_without_llm`, `dispatch_executable` [slash.rs:dispatch_without_llm]
- Background runs: `BackgroundAgentRegistry`, `BackgroundAgentListEntry` [background.rs:BackgroundAgentRegistry]; `spawn_background_agent`, `format_background_result`, `persist_background_result` [background_run.rs:spawn_background_agent]
- Activity: `ActivityRegistry`, `ActivityEntry`, `ActivityStatus`, `ActivityHandle`, `ActivityToken`, `LANE_SUBAGENT`, `truncate_note` [activity.rs:ActivityRegistry]
- `RecordingOutput`, `OutputEvent`, `RecordedEvent` - capture-only `Output` [recording.rs:RecordingOutput]
- `TerminalController`, `TerminalOutput` [terminal.rs:TerminalController]
- `read_log_tail(n)` [log_tail.rs:read_log_tail]
- [http](./http/EGM.md) - web UI, JSON API, SSE
- [telegram](./telegram/EGM.md) - Telegram bot transport

## Invariants
- `Public` agents get only `workspace`, `memory_search`, `web_fetch`, `web_search` (no filesystem, shell, MCP, or subagents) on a per-channel workspace, with the sandbox always enforced [agent_builder.rs:PUBLIC_AGENT_TOOLS, mod.rs:AgentMode]
- At most `MAX_BACKGROUND_AGENTS = 4` concurrent background agents [background.rs:MAX_BACKGROUND_AGENTS]
- Activity registry keeps up to 64 entries per chat and treats running entries older than 1 hour as stale [activity.rs:CAP_PER_CHAT, activity.rs:STALE_RUNNING_SECS]
- Log tails are read in 8 KiB chunks from the end, never loading whole files [log_tail.rs:read_log_tail_from_dir]

## Side Effects
- Process-global settings bus and browser-artefact sink [runtime.rs:install_settings_bus, runtime.rs:install_browser_artefact_sink]
- Spawns background agent tasks and appends their results to chat history [background_run.rs:spawn_background_agent, background_run.rs:persist_background_result]
- Reads logs under `$HOME/.dyson` [log_tail.rs:read_log_tail]
- Terminal controller reads stdin and writes stdout [terminal.rs:TerminalController]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
