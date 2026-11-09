# Tools (`tool`)

## Overview
The `Tool` trait and Dyson's built-in tool implementations. A tool is a stateless capability with a JSON input schema, an execution plan (resource claims and idempotency) used by the harness for leasing and recovery, and an async `run` returning `ToolOutput`. `ToolContext` carries the working directory, sandbox, workspace, environment, depth, and caches; path helpers confine file access to the workspace unless the sandbox is explicitly bypassed.

## Interface
- `Tool` trait: `name`, `description`, `input_schema`, `idempotency_key`, `lookup_result`, `agent_only`, `execution_plan`, `run(input, ctx) -> Result<ToolOutput>` [mod.rs:Tool]
- `ToolContext`, `ToolOutput`, `CheckpointEvent` [mod.rs:ToolContext, mod.rs:ToolOutput, mod.rs:CheckpointEvent]
- Re-exports `Idempotency`, `ResourceAccess`, `ResourceClaim`, `ToolExecutionPlan` from `dyson_harness` [mod.rs]
- Path helpers: `resolve_and_validate_path`, `SandboxedPath`, `validate_workspace_path`, `file_resource_key` [mod.rs:resolve_and_validate_path, mod.rs:validate_workspace_path]
- `MAX_SUBAGENT_DEPTH = 3` [mod.rs:MAX_SUBAGENT_DEPTH]
- Tools (registered names): `bash` [bash.rs:BashTool], `read_file` [read_file.rs:ReadFileTool], `write_file` [write_file.rs], `edit_file` [edit_file.rs:EditFileTool], `list_files` [list_files.rs:ListFilesTool], `search_files` [search_files.rs:SearchFilesTool], `send_file` [send_file.rs:SendFileTool], `workspace` [workspace.rs], `memory_search` [memory_search.rs:MemorySearchTool], `kb_search` [kb_search.rs:KbSearchTool], `kb_status` [kb_status.rs:KbStatusTool], `load_skill` [load_skill.rs:LoadSkillTool], `artifacts` [artefacts.rs:ArtefactsTool], `skill_marketplace` [skill_marketplace.rs:SkillMarketplaceTool], `web_fetch` [web_fetch.rs], `web_search` [web_search.rs], `image_generate` [image_generate.rs:ImageGenerateTool], `dependency_scan` [dependency_scan.rs:DependencyScanTool], `export_conversation` [export_conversation.rs:ExportConversationTool], `request_human_input` [human_input.rs:HumanInputTool]
- `ArtefactReader` trait, `ArtefactSummary`, `ArtefactRecord`, `ArtefactLookup` [artefacts.rs:ArtefactReader]
- `ImageGenerationProvider` trait, `GeminiImageProvider`, `OpenRouterImageProvider`, `create_provider` [image_generate.rs:ImageGenerationProvider]
- `install_skill_package_to_workspace`, `remove_skill_from_workspace`, `SkillBody`, `SkillInstallError`, `SkillRemoveError` [skill_marketplace.rs:install_skill_package_to_workspace]
- `ToolView` and UI view rows (`TermLine`, `DiffFile`, `DiffRow`, `SbomRow`, `SbomCounts`) [view.rs:ToolView]
- `is_safe_env_var` (crate) [bash.rs:is_safe_env_var]
- [bulk_edit](./bulk_edit/EGM.md) - multi-file rename / find-replace / definitions
- [security](./security/EGM.md) - AST, taint, attack-surface, exploit tools
- [skill_create](./skill_create/EGM.md) - create/improve workspace skills

## Invariants
- Paths are canonicalised (nearest existing ancestor for new files) and must stay inside the workspace unless a sandbox bypass is in effect [mod.rs:resolve_and_validate_path]
- Workspace paths reject absolute paths and traversal [mod.rs:validate_workspace_path]
- `ToolContext` refuses deeper nesting when `depth >= MAX_SUBAGENT_DEPTH` [mod.rs:ToolContext]
- `bash`: default timeout 120 s with process-group kill; output truncated to `MAX_OUTPUT_BYTES`; per-stream reads capped at 5 MiB; child env cleared and restricted to `is_safe_env_var` names [bash.rs:BashTool, bash.rs:MAX_READ_BYTES, bash.rs:is_safe_env_var]
- Size limits: `read_file` 50 MB and extraction <= 16 MiB with a 50x expansion-ratio check [read_file.rs:MAX_EXTRACTED_BYTES, read_file.rs:MAX_EXTRACT_RATIO]; `edit_file` 10 MiB [edit_file.rs:MAX_FILE_SIZE]; `list_files` 1000 results [list_files.rs:MAX_RESULTS]; `search_files` 500 matches [search_files.rs:MAX_MATCHES]; `image_generate` prompt <= 10,000 chars [image_generate.rs:MAX_PROMPT_LEN]
- `web_fetch` validates URLs with `crate::http::validate_url_safe` (SSRF guard), URL <= 2048 chars, body <= 32 MiB, output length clamped 1000..=200,000 [web_fetch.rs:MAX_URL_LENGTH, web_fetch.rs:MAX_BODY_BYTES, web_fetch.rs:MAX_MAX_LENGTH]

## Side Effects
- Spawns `bash -c` subprocesses [bash.rs:BashTool]
- Reads and writes files under the working directory [edit_file.rs:EditFileTool, write_file.rs]
- Outbound HTTP for `web_fetch`, `web_search`, and image providers (`https://openrouter.ai/api` default for OpenRouter) [web_fetch.rs, web_search.rs, image_generate.rs:OPENROUTER_DEFAULT_BASE]
- Writes generated images and exported conversations to disk [image_generate.rs:ImageGenerateTool, export_conversation.rs:ExportConversationTool]
- Installs/removes skill packages in the workspace [skill_marketplace.rs:install_skill_package_to_workspace]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
