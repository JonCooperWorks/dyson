# Skill Create Tool (`tool/skill_create`)

## Overview
The `skill_create` tool lets the agent create or improve workspace skills: it writes `skills/<name>/SKILL.md` (YAML frontmatter `name`/`description` plus instructions), an optional `dyson-skill.json` manifest, and optional script code for hybrid skills with a slash command. Skills are picked up on the next reload.

## Interface
- `SkillCreateTool` - `Tool` named `skill_create`; input `name`, `description`, `instructions`, `mode` (`create` default | `update` | `improve`), optional `slash_command`, optional `execution` [mod.rs:SkillCreateTool]

## Invariants
- Reuses `validate_skill_name`, `validate_slash_command`, `validate_entrypoint`, `normalize_timeout_ms` from `skill::local` [mod.rs:SkillCreateTool]
- `create` fails if the skill already exists [mod.rs:SkillCreateTool]
- Script skills require a `slash_command`, and `execution.code` when creating [mod.rs:SkillCreateTool]
- Requires a workspace (`ctx.workspace("skill_create")`) [mod.rs:SkillCreateTool]

## Side Effects
- Writes `skills/<name>/SKILL.md`, `skills/<name>/dyson-skill.json`, and `skills/<name>/<entrypoint>` via `ws.set` and persists with `ws.save()` [mod.rs:SkillCreateTool]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
