# Config Loader Internals (`config/loader`)

## Overview
Submodules of `config/loader.rs` that turn `dyson.json` into runtime `Settings`: file discovery, size/permission checks, migration and write-back (`files.rs`); serde schema types (`schema.rs`); conversion into `Settings` (`parsing.rs`); secret-reference resolution and credential-destination checks (`secrets.rs`); and cross-reference validation (`validation.rs`).

## Interface
- `persist_model_selection(config_path, provider_name, model)` - re-exported `pub` from `loader.rs` [files.rs:persist_model_selection]
- `validate_subagent_configs(settings)` - re-exported `pub` [validation.rs:validate_subagent_configs]
- `read_config_file`, `tighten_config_perms`, `try_discover_config`, `load_and_migrate`, `write_back_config` [files.rs:try_discover_config]
- `JsonRoot` and nested `JsonAgent`, `JsonProviderConfig`, `JsonSkills`, `JsonLocalSkill`, `JsonBuiltinSkill`, `JsonSubagent`, `JsonSandbox`, `JsonCedar`, `JsonToolPolicy`, `JsonWorkspace`, `JsonMemory`, `JsonChatHistory`, `JsonTranscriber`, `JsonWebSearch`, `JsonRateLimit`, `JsonCompaction` [schema.rs:JsonRoot]
- `build_settings(json_root, secrets) -> Settings` and per-section parsers `parse_providers`, `parse_agent_settings`, `parse_compaction`, `parse_skills`, `parse_mcp_servers`, `parse_mcp_oauth`, `parse_sandbox`, `parse_workspace`, `parse_chat_history`, `parse_transcriber`, `parse_web_search`, `parse_controllers`, `parse_tool_policy` [parsing.rs:build_settings]
- `resolve_secrets_in_value`, `resolve_api_keys`, `reject_http_with_api_key`, `is_local_network_host` [secrets.rs:resolve_api_keys]
- `validate_agent_model(settings)` [validation.rs:validate_agent_model]

## Invariants
- Config files larger than `MAX_CONFIG_SIZE` (1 MiB) are rejected [files.rs:MAX_CONFIG_SIZE, files.rs:read_config_file]
- Discovery order: `./dyson.json`, then `$HOME/.config/dyson/dyson.json`, else `None` (defaults) [files.rs:try_discover_config]
- An API key may not be sent over plain `http://` unless the host is loopback/private/link-local/CGNAT (via `crate::http::is_private_v4/v6`); DNS names count as remote [secrets.rs:reject_http_with_api_key, secrets.rs:is_local_network_host]
- An empty `agent.model` is a configuration error; there is no hardcoded default model [validation.rs:validate_agent_model]
- Subagent `provider` must be `"default"` or a known provider [validation.rs:validate_subagent_configs]
- A missing `skills.builtin` section yields a builtin skill with an empty tool list (meaning all defaults) [parsing.rs:parse_skills]; MCP servers become `SkillConfig::Mcp` entries [parsing.rs:parse_mcp_servers]

## Side Effects
- Reads config files and `HOME` [files.rs:try_discover_config]
- Writes migrated config back to disk and chmods it `0o600` on Unix [files.rs:write_back_config, files.rs:tighten_config_perms]
- Rewrites `agent.provider` / `agent.model` in the config file [files.rs:persist_model_selection]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
