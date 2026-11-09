# Local Skills (`skill/local`)

## Overview
Loads workspace skills from `skills/<name>/SKILL.md` (freeform instructions with optional frontmatter) plus an optional `dyson-skill.json` manifest. Skill bodies are not injected into the system prompt; only name and description appear in `<available_skills>`, and the full body is fetched on demand by the `load_skill` tool. Hybrid skills may declare a script entrypoint exposed as a slash command and as a `skill_<name>` tool that runs the script with bash.

## Interface
- `LocalSkill` - implements `Skill`; `skill_description()`, body accessors, script detection [mod.rs:LocalSkill]
- `SkillListSkill` / `SkillListEntry` - the `skill-list` skill that advertises available skills [mod.rs:SkillListSkill]
- `LocalScriptSkillTool` - `Tool` that runs a skill script [script.rs:LocalScriptSkillTool]
- `manifest` (pub): `LocalSkillManifest`, `SkillExecution { None | Script }`, `ScriptExecution`, `ArgumentMode` [manifest.rs:LocalSkillManifest]
- Validators/helpers: `validate_skill_name`, `validate_slash_command`, `validate_entrypoint`, `resolved_entrypoint`, `normalize_timeout_ms`, `tool_name_for_skill` [manifest.rs:validate_skill_name]
- Constants: `DEFAULT_SCRIPT_TIMEOUT_MS = 30_000`, `MAX_SCRIPT_TIMEOUT_MS = 300_000`, `RESERVED_SLASH_COMMANDS` (`/agents`, `/clear`, `/compact`, `/fork-from`, `/logs`, `/loop`, `/model`, `/models`, `/stop`) [manifest.rs:RESERVED_SLASH_COMMANDS]

## Invariants
- Skill names are lowercase letters, digits, and hyphens; the manifest `name` must match the directory name [manifest.rs:validate_skill_name, manifest.rs:LocalSkillManifest]
- Unsupported manifest `schema_version` or execution kind is a config error; script execution requires a `slash_command` and an existing entrypoint [manifest.rs:LocalSkillManifest]
- Entrypoints must be non-empty, relative, contain no NUL or backslash, and only normal path components (no `..`, `.`, root) [manifest.rs:validate_entrypoint]
- Timeouts are clamped to `1..=MAX_SCRIPT_TIMEOUT_MS` [manifest.rs:normalize_timeout_ms]; tool name is `skill_` + name with `-` replaced by `_` [manifest.rs:tool_name_for_skill]
- `SKILL.md` content is capped at `MAX_SKILL_CONTENT_SIZE = 64 KiB`; an empty file is rejected, any non-empty file is loadable [mod.rs:MAX_SKILL_CONTENT_SIZE, mod.rs:LocalSkill]
- Script output reads are capped at `MAX_READ_BYTES = 5 MiB` per stream [script.rs:MAX_READ_BYTES]

## Side Effects
- Reads `SKILL.md` and `dyson-skill.json` from disk [mod.rs:LocalSkill]
- Spawns `bash <entrypoint>` in the skill directory with a cleared environment re-populated only with `is_safe_env_var` variables; kills the child on timeout [script.rs:LocalScriptSkillTool]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
