# Tools & Skills

Tools are the fundamental unit of capability in Dyson.  Skills bundle tools
with lifecycle hooks and prompt fragments.  Together, they form the
extensibility layer that lets you plug arbitrary capabilities into the agent.

**Key files:**
- `src/tool/mod.rs` — `Tool` trait, `ToolContext`, `ToolOutput`
- `src/tool/bash.rs` — `BashTool` (shell execution with timeout)
- `src/tool/web_search.rs` — `WebSearchTool`, `SearchProvider` trait, Brave/SearXNG providers
- `src/skill/mod.rs` — `Skill` trait, `create_skills()` factory
- `src/skill/builtin.rs` — `BuiltinSkill` (wraps built-in tools)
- `src/skill/local.rs` — `LocalSkill` (SKILL.md parser, workspace discovery)

---

## Tool Trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput>;
}
```

| Method | Purpose |
|--------|---------|
| `name()` | Unique identifier for dispatch and display (e.g., `"bash"`) |
| `description()` | Shown to the LLM so it knows when to use this tool |
| `input_schema()` | JSON Schema the LLM uses to construct valid input |
| `run()` | Execute the tool — async because tools do I/O |

Object-safe via `async_trait`. Tools are stored as `Arc<dyn Tool>` — shared between skills and the agent's flat lookup map.

---

## ToolContext

Shared context passed to every tool: `working_dir` (CWD), `env` (environment variables), `cancellation` (Ctrl-C token). Created once at startup.

---

## ToolOutput

- `content: String` — sent to LLM as `tool_result`
- `is_error: bool` — LLM can retry or adjust
- `metadata: Option<Value>` — internal only (timing, exit codes)

`ToolOutput { is_error: true }` = tool ran but failed. `Err(DysonError)` = tool couldn't run at all. Both become `tool_result` blocks.

---

## BashTool

Spawns `bash -c "<command>"` with the tool context's working directory and env vars. Input: `{ "command": "string" }`.

- **Timeout**: 120s default; returns error on timeout
- **Truncation**: Output capped at 100KB (UTF-8 aware) to protect context window
- **Output**: stdout + stderr combined; `is_error = exit code != 0`

---

## Skill Trait

```rust
#[async_trait]
pub trait Skill: Send + Sync {
    fn name(&self) -> &str;
    fn tools(&self) -> &[Arc<dyn Tool>];
    fn system_prompt(&self) -> Option<&str> { None }
    async fn on_load(&mut self) -> Result<()> { Ok(()) }
    async fn after_tool(&self, name: &str, result: &ToolOutput) -> Result<()> { Ok(()) }
    async fn on_unload(&mut self) -> Result<()> { Ok(()) }
}
```

| Method | Required | Purpose |
|--------|----------|---------|
| `name()` | Yes | Unique skill identifier for logging |
| `tools()` | Yes | The tools this skill provides (`&[Arc<dyn Tool>]`) |
| `system_prompt()` | No | Prompt fragment appended to the base system prompt |
| `on_load()` | No | Setup: connect to servers, read config files |
| `after_tool()` | No | Post-process tool results (logging, metrics) |
| `on_unload()` | No | Cleanup: close connections, kill child processes |

### Lifecycle

`on_load()` → `tools()` + `system_prompt()` registered → `after_tool()` per execution → `on_unload()` on shutdown.

Skills add what tools alone lack: grouping, setup/teardown lifecycle, prompt context, and post-processing hooks.

---

## BuiltinSkill

The default skill wrapping Dyson's built-in tools:
- `BashTool` — shell command execution with timeout
- `ReadFileTool` — read workspace files with optional line range
- `WriteFileTool` — create or overwrite files
- `EditFileTool` — pattern-based find-and-replace editing
- `ListFilesTool` — glob-based file discovery
- `SearchFilesTool` — regex content search across files
- `MemorySearchTool` — full-text search over memory files
- `WorkspaceViewTool` — view/list workspace files
- `WorkspaceSearchTool` — search across workspace files
- `WorkspaceUpdateTool` — update workspace files (set/append)
- `LoadSkillTool` — on-demand skill loading
- `SkillCreateTool` — create, update, or improve skills
- `SendFileTool` — send file to user via controller
- `ExportConversationTool` — export chat history in ShareGPT format
- `WebSearchTool` — web search via pluggable provider (conditional — see below)

The system prompt is generated dynamically from the loaded tools — each
tool's name and description are listed so the LLM knows what's available.

---

## WebSearchTool

Pluggable web search via the `SearchProvider` trait. Only appears when `web_search` is configured in `dyson.json`.

| Provider | Config value | Requires |
|----------|-------------|----------|
| Brave Search | `"brave"` | `api_key` |
| SearXNG | `"searxng"` | `base_url` |

Input: `{ "query": "string", "num_results": 1-10 }`. See [Configuration](configuration.md#web-search) for setup.

---

## Adding a New Tool

1. Create `src/tool/my_tool.rs` — implement `Tool` trait (`name`, `description`, `input_schema`, `run`)
2. Add `pub mod my_tool;` to `src/tool/mod.rs`
3. Add the tool to `BuiltinSkill::new()` in `src/skill/builtin.rs`

See `src/tool/bash.rs` as a template. The agent discovers tools automatically via skills.

---

## Skill Taxonomy

| Skill | Status | Tools | Source |
|-------|--------|-------|--------|
| `BuiltinSkill` | Implemented | bash, read/write/edit_file, list/search_files, workspace_*, memory_search, web_search, load_skill, skill_create, send_file, export_conversation | Compiled into Dyson |
| `McpSkill` | Implemented | Discovered via `tools/list` | MCP server (stdio/HTTP) |
| `LocalSkill` | Implemented | None (system prompt list only) | skills/*/SKILL.md |
| `SubagentSkill` | Implemented | Subagent tools (planner, researcher, user-defined) | Config + parent tools |
| `SkillListSkill` | Implemented | None (system prompt only) | Generated from discovered skills |

All skill types implement the same `Skill` trait.  The agent loop treats them
identically.

---

## LocalSkill — Workspace-Managed Skills

Skills live in `~/.dyson/skills/<name>/SKILL.md` and are auto-discovered at startup.

### Two-Phase Loading

1. **Startup**: Scan frontmatter (name + description) → build `<available_skills>` list in system prompt. Full body is NOT injected.
2. **Runtime**: LLM calls `load_skill("name")` to fetch full instructions on demand.

### SKILL.md Format

```markdown
---
name: code-review
description: Reviews code for quality and security issues
---

You are a code review expert. When asked to review code:
1. Search the workspace for the relevant files
2. Analyze code quality, security, and patterns
3. Provide actionable feedback
```

Frontmatter requires `name`; `description` is optional. Body is loaded on demand.

### Discovery & Tools

- **Auto-scan**: `~/.dyson/skills/*/SKILL.md`
- **Config**: `skills.local` in `dyson.json` with explicit paths
- **`load_skill`**: fetch full instructions by name
- **`skill_create`**: create/update/improve skills (modes: `create`, `update`, `improve`)

Hot reload watches `skills/` for changes. Failed skills are logged and skipped — they never stop the agent.

---

See also: [Architecture Overview](architecture-overview.md) ·
[Agent Loop](agent-loop.md) · [Sandbox](sandbox.md) ·
[Tool Execution Pipeline](tool-execution-pipeline.md) ·
[Tool Forwarding over MCP](tool-forwarding-over-mcp.md)
