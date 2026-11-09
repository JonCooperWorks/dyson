# Skills (`skill`)

## Overview
The `Skill` trait is Dyson's primary extension point: a bundle of tools with optional lifecycle hooks and a system-prompt fragment. The agent sees only skills, never individual tool sources. `BuiltinSkill` wraps the built-in tools; `McpSkill`, `LocalSkill`, and `SubagentSkill` (in subdirectories) provide remote, workspace, and child-agent tools. `create_skills` builds every configured skill in two phases.

## Interface
- `Skill` trait: `name()`, `tools() -> &[Arc<dyn Tool>]`, `system_prompt()`, `on_load()`, `after_tool(tool_name, result)`, `before_turn()`, `on_unload()` [mod.rs:Skill]
- `create_skills(...) -> Vec<Box<dyn Skill>>` [mod.rs:create_skills]
- `BuiltinSkill` - `new_filtered(web_search_config, image_provider_config, image_model_override, filter)`, `Default` [builtin.rs:BuiltinSkill]
- [local](./local/EGM.md) - `SKILL.md` workspace skills and script-backed slash commands
- [mcp](./mcp/EGM.md) - MCP client skill (and `serve/` MCP server)
- [subagent](./subagent/EGM.md) - child agents and orchestrator harnesses

## Invariants
- Phase A loads builtin, MCP, and local skills and auto-discovers `skills/` in the workspace into a `SkillListSkill`; Phase B builds `SubagentSkill` from the flattened tools of the already-loaded skills [mod.rs:create_skills]
- Built-in tools: `bash`, `read_file`, `write_file`, `edit_file`, `bulk_edit`, `list_files`, `search_files`, `send_file`, `memory_search`, `workspace`, `load_skill`, `artefacts`, `skill_create`, skill marketplace, `kb_search`, `kb_status`, `web_fetch`, `dependency_scan`, and the AST security tools; `web_search` and `image_generate` are added only when their providers build successfully [builtin.rs:BuiltinSkill::new_filtered]
- A non-empty `filter` keeps only the named tools; an empty filter keeps all [builtin.rs:BuiltinSkill::new_filtered]

## Side Effects
- Skill loading may connect to MCP servers, read workspace skill files, and log provider build failures [mod.rs:create_skills, builtin.rs:BuiltinSkill::new_filtered]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
