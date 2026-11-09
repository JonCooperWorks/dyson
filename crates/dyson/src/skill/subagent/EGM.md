# Subagent Skill (`skill/subagent`)

## Overview
Spawns child agents as tools. A `SubagentTool` builds a fresh child `Agent` with its own LLM client, system prompt, and conversation, runs it to completion, and returns only the final text. `OrchestratorTool` composes a child with filtered direct tools plus inner subagents (planner, researcher, coder, verifier) for parallel dispatch, and hosts the `security_engineer` and `pentester` harnesses. System prompts are `include_str!`-bundled from `prompts/`.

## Interface
- `SubagentTool`, `SubagentSkill` (`Skill` contributing subagent tools and a prompt fragment) [mod.rs:SubagentTool, mod.rs:SubagentSkill]
- `CaptureOutput` - `Output` that swallows the child's intermediate events [mod.rs:CaptureOutput]
- `FilteredSkill` - wraps inherited `Arc<dyn Tool>` clones for a child [mod.rs:FilteredSkill]
- `ChildSpawn`, `spawn_child(spec) -> Result<ToolOutput>` (crate) [mod.rs:spawn_child]
- `builtin_subagent_configs()` - `planner`, `researcher`, `verifier`, `dependency_review` [mod.rs:builtin_subagent_configs]
- `builtin_orchestrator_configs()` - `security_engineer_config()`, `pentester_config()` [mod.rs:builtin_orchestrator_configs]
- `OrchestratorConfig`, `OrchestratorHarness { SecurityResearch | PenetrationTest }`, `OrchestratorTool` (input `{ task, context? }`) [orchestrator.rs:OrchestratorConfig, orchestrator.rs:OrchestratorTool]
- `OrchestratorInput`, `pentest_preflight_schema`, `apply_pentest_preflight_result` (crate) [orchestrator.rs:OrchestratorInput]
- `CoderTool` - `coder` subagent tool [coder.rs:CoderTool]
- `pentester_config()` - `pentester`, `max_iterations: 80`, `max_tokens: 8192`, emits a `SecurityReview` artefact [pentester.rs:pentester_config]
- [repo_detect](./repo_detect/EGM.md) - manifest-based language/framework detection
- [security_engineer](./security_engineer/EGM.md) - staged security research harness
- `prompts/` - Markdown system prompts and briefings (not code)

## Invariants
- Children inherit the parent's `Arc<dyn Sandbox>` and workspace; delegation cannot bypass the sandbox [mod.rs]
- Only the child's final text reaches the parent conversation [mod.rs:CaptureOutput]
- Subagent tools are excluded from children, and `spawn_child` refuses when `parent_depth >= MAX_SUBAGENT_DEPTH` [mod.rs:spawn_child]
- Tools are inherited as `Arc` clones; MCP connections stay owned by the parent's `McpSkill` [mod.rs:SubagentTool]
- `coder` may use only `bash`, `read_file`, `edit_file`, `search_files`, `list_files`, `bulk_edit` [coder.rs:CODER_TOOLS]
- `pentester` direct tools: `bash`, `read_file`, `search_files`, `list_files`, `web_fetch`, `ast_describe`, `ast_query`, `attack_surface_analyzer`, `exploit_builder`, `taint_trace`, `dependency_scan` [pentester.rs:DIRECT_TOOLS]
- Penetration tests require a preflight elicitation confirming target, authority, and scope, which needs the elicitation UI [orchestrator.rs:complete_pentest_preflight]
- `PENTEST_EXECUTION_TIMEOUT_MS = 1_800_000` [orchestrator.rs:PENTEST_EXECUTION_TIMEOUT_MS]

## Side Effects
- Builds and runs child agents with their own LLM calls [mod.rs:spawn_child]
- Raises elicitations through the process-global MCP broker [orchestrator.rs:complete_pentest_preflight]
- Writes harness reports to `kb/security-harness/reports/{run_id}.json` (or under the working directory) [orchestrator.rs:OrchestratorTool]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
