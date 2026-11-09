# HTTP Admin Endpoint Modules (`routes/admin`)

## Overview
Submodules of `routes/admin.rs` that implement the Swarm-facing admin surface under `/api/admin/*`: configure-secret enrollment and verification, atomic `dyson.json` patching, durable state-file restore, skill package install/remove/diagnostics, fleet lifecycle (idle/quiesce), and message cost backfill. `admin.rs` re-exports the handlers to the router in `routes/mod.rs`.

## Interface
- `authorize_configure(headers, state)` - validates the per-instance configure secret header (`DYSON_CONFIGURE_HEADER`) against the stored hash; first use enrolls it [auth.rs:authorize_configure]
- `preseed_configure_hash(dyson_home)` - boot-time conversion of a `configure.preseed` secret into the stored hash; re-exported publicly from `admin.rs` [auth.rs:preseed_configure_hash]
- `prepare_http_auth(...)` [auth.rs:prepare_http_auth], `CONFIGURE_HASH_FILENAME = "configure_secret_hash"` [auth.rs:CONFIGURE_HASH_FILENAME]
- `ConfigureConfigPatch`, `AppliedConfigPatch`, `ConfigureConfigPatchError`, `patch_config_once(...)` - single read / patch / atomic write of the config document [config.rs:patch_config_once]
- Document patch helpers: `patch_subscription_models_doc`, `patch_provider_doc`, `patch_active_selection_doc`, `patch_image_generation_doc`, `clear_skills_doc`, `set_skills_tools_doc`, `patch_mcp_servers_doc`, `patch_telegram_controller_doc`, `write_config_doc` [config.rs:write_config_doc]
- `post_state_file` (`POST /api/admin/state/file`), `clean_relative_path` [state_files.rs:post_state_file, state_files.rs:clean_relative_path]
- `post_skill_install` (`POST /api/admin/skills/install`), `delete_skill` (`DELETE /api/admin/skills/:skill`), `get_skills` (`GET /api/admin/skills`) [skills.rs:post_skill_install, skills.rs:delete_skill, skills.rs:get_skills]
- `get_idle` (`GET /api/admin/idle`), `post_quiesce` (`POST /api/admin/quiesce`), `post_unquiesce` (`POST /api/admin/unquiesce`) [lifecycle.rs:get_idle, lifecycle.rs:post_quiesce, lifecycle.rs:post_unquiesce]
- `post_cost_backfill` (`POST /api/admin/cost-backfill`), `CostBackfillBody` [costs.rs:post_cost_backfill]

## Invariants
- Configure secret: if no hash exists, a non-empty `configure.preseed` must match the presented secret, otherwise the operator bootstrap bearer must authenticate before trust-on-first-use enrollment; the secret is hashed with argon2 [auth.rs:authorize_configure]
- Successful verifications are cached for `CONFIGURE_VERIFY_CACHE_TTL` = 30 s [auth.rs:CONFIGURE_VERIFY_CACHE_TTL]
- State restore accepts only namespaces `workspace` and `chats`; paths must be non-empty, <= 2048 bytes, no NUL, relative, slash-separated, and contain only normal components [state_files.rs:state_root, state_files.rs:clean_relative_path]
- Body caps: `MAX_STATE_FILE_BODY` 8 MiB [state_files.rs:MAX_STATE_FILE_BODY], `MAX_SKILL_INSTALL_BODY` 96 KiB [skills.rs:MAX_SKILL_INSTALL_BODY], `MAX_COST_BACKFILL_BODY` 1024 bytes [costs.rs:MAX_COST_BACKFILL_BODY]
- `set_skills_tools_doc` fails with `SkillsNotObject` / `SkillsBuiltinNotObject` when the `skills` shape is invalid [config.rs:set_skills_tools_doc]
- Cost backfill requires a configured chat-history backend [costs.rs:post_cost_backfill]

## Side Effects
- Writes `configure_secret_hash` with mode `0o600` beside the workspace directory and deletes `configure.preseed` after use [auth.rs:authorize_configure, auth.rs:preseed_configure_hash]
- Rewrites `dyson.json` atomically via temp file + `tokio::fs::rename` [config.rs:write_config_doc]
- Writes or deletes restored state files atomically (temp + rename) under the workspace or chat-history root [state_files.rs:post_state_file]
- Installs/removes skill packages in the workspace via `crate::tool::skill_marketplace` [skills.rs:post_skill_install, skills.rs:delete_skill]
- `get_skills` connects to each configured MCP server (`McpSkill::on_load`) to report live tool inventory [skills.rs:get_skills]
- Quiesce/unquiesce toggles HTTP state so new turns are refused during fleet operations [lifecycle.rs:post_quiesce]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
