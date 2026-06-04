# Tools & Skills

Tools are the fundamental unit of capability in Dyson.  Skills bundle tools
with lifecycle hooks and prompt fragments.  Together, they form the
extensibility layer that lets you plug arbitrary capabilities into the agent.

**Key files:**
- `src/tool/mod.rs` — `Tool` trait, `ToolContext`, `ToolOutput`
- `src/tool/bash.rs` — `BashTool` (shell execution with timeout)
- `src/tool/web_fetch.rs` — `WebFetchTool` (URL fetching with HTML-to-text extraction)
- `src/tool/web_search.rs` — `WebSearchTool`, `SearchProvider` trait, Brave/SearXNG providers
- `src/skill/mod.rs` — `Skill` trait, `create_skills()` factory
- `src/skill/builtin.rs` — `BuiltinSkill` (wraps built-in tools)
- `src/skill/local.rs` — `LocalSkill` (SKILL.md parser, workspace discovery)
- `src/ast/` — shared tree-sitter grammars and walking helpers; consumed by `bulk_edit`, `read_file`, `search_files`, and security tools (see [AST docs](ast.md))
- `src/tool/bulk_edit/` — `BulkEditTool` (unified multi-file edit: AST rename, find_replace, list_definitions)
- `src/tool/read_file.rs` — `ReadFileTool`; supports `symbol` extraction for AST-aware single-definition reads
- `src/tool/search_files.rs` — `SearchFilesTool`; supports `ast: true` for identifier-only searches

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

| Tool | Availability | Purpose |
|---|---|---|
| `bash` | default | Shell command execution with timeout |
| `read_file` | default | Read workspace files with optional line or symbol selection |
| `write_file` | default | Create or overwrite files |
| `edit_file` | default | Pattern-based single-file edits |
| `bulk_edit` | default | Multi-file AST rename, find/replace, and definition listing |
| `list_files` | default | Glob-based file discovery |
| `search_files` | default | Regex or AST-aware content search |
| `send_file` | default | Send a file through the active controller |
| `memory_search` | default | Full-text search over memory files |
| `workspace` | default | Unified view/list/search/update for workspace files |
| `load_skill` | default | Load full instructions for a discovered local skill |
| `skill_marketplace` | default | List, inspect, install, update, and remove marketplace skills |
| `kb_search` | default | Search the workspace knowledge base |
| `kb_status` | default | Report knowledge-base status and index coverage |
| `web_fetch` | default | Fetch a URL and return extracted text |
| `dependency_scan` | default | Detect dependency manifests and query vulnerability data |
| `ast_describe` | default | Describe AST structure for supported source files |
| `ast_query` | default | Query AST nodes by language and pattern |
| `attack_surface_analyzer` | default | Summarize security-relevant entrypoints and flows |
| `exploit_builder` | default | Build evidence-oriented exploit sketches for review work |
| `taint_trace` | default | Trace selected source-to-sink flows across supported languages |
| `web_search` | conditional | Web search via configured Brave or SearXNG provider |
| `image_generate` | conditional | Image generation via configured image-capable provider |

The system prompt is generated dynamically from the loaded tools — each
tool's name and description are listed so the LLM knows what's available.

---

## WebFetchTool

Fetches a URL and returns clean extracted text. HTML pages are stripped of tags, scripts, and styles via `nanohtml2text`. Also handles `text/plain` and `application/json` (pretty-printed). Always available — no configuration needed.

Input: `{ "url": "https://...", "max_length": 50000 }`.

---

## WebSearchTool

Pluggable web search via the `SearchProvider` trait. Only appears when `web_search` is configured in `dyson.json`.

| Provider | Config value | Requires |
|----------|-------------|----------|
| Brave Search | `"brave"` | `api_key` |
| SearXNG | `"searxng"` | `base_url` |

Input: `{ "query": "string", "num_results": 1-10 }`. See
[Configuration](configuration.md#web-search-and-transcription) for setup.

## ImageGenerateTool

`image_generate` is registered only when
`agent.image_generation_provider` points at a provider that supports image
generation. Today that is Gemini or OpenRouter; swarm-managed instances
automatically receive a dedicated OpenRouter image provider when swarm pushes
runtime config.
Generated files are emitted through the active controller as artefacts.

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
| `BuiltinSkill` | Implemented | bash, file/search/edit tools, workspace, memory, KB, web fetch/search, dependency and AST/security tools, marketplace tools, optional image generation | Compiled into Dyson |
| `McpSkill` | Implemented | Discovered via `tools/list` | MCP server (stdio/HTTP) |
| `LocalSkill` | Implemented | None (listed in prompt; loaded through `load_skill`) | skills/*/SKILL.md |
| `SubagentSkill` | Implemented | Subagent tools (planner, researcher, user-defined) | Config + parent tools |
| `SkillListSkill` | Implemented | None (system prompt only) | Generated from discovered skills |

All skill types implement the same `Skill` trait.  The agent loop treats them
identically.

---

## LocalSkill — Workspace-Managed Skills

Skills live in `~/.dyson/skills/<name>/SKILL.md` and are auto-discovered at startup.

### Two-Phase Loading

1. **Startup**: Scan `skills/<name>/SKILL.md`; the directory name is the skill name and optional frontmatter supplies the description. Full body is NOT injected.
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

The directory name is authoritative for the skill name. `description` is
optional frontmatter. Body is loaded on demand.

### Discovery & Tools

- **Auto-scan**: `~/.dyson/skills/*/SKILL.md`
- **Config**: `skills.local` in `dyson.json` with explicit paths
- **`load_skill`**: fetch full instructions by name
- **Background learning**: self-improvement paths can call `skill_create` to
  create, update, or improve workspace skills.

Failed skills are logged and skipped — they never stop the agent.

### Hot Reload

Skills hot-reload within the same session.  The `HotReloader`
(`src/config/hot_reload.rs`) watches the `skills/` directory and all
existing `SKILL.md` files by mtime.  Before each user turn the controller
calls `check_and_reload_agent()` — if any skill file changed, the agent
is rebuilt with fresh skills.  Conversation messages are preserved across
the rebuild.

This means skills created by the `SelfImprovementDream` (or by the agent
the self-improvement path via `skill_create`) are active by the next turn — no restart
needed.  See [Dreaming](dreaming.md#skill-creation-and-hot-reload) for
the full lifecycle.

---

See also: [Architecture Overview](architecture-overview.md) ·
[Agent Loop](agent-loop.md) · [Sandbox](sandbox.md) ·
[Tool Execution Pipeline](tool-execution-pipeline.md) ·
[Tool Forwarding over MCP](tool-forwarding-over-mcp.md) ·
[Elicitation](elicitation.md)
