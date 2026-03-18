# Tools & Skills

Tools are the fundamental unit of capability in Dyson.  Skills bundle tools
with lifecycle hooks and prompt fragments.  Together, they form the
extensibility layer that lets you plug arbitrary capabilities into the agent.

**Key files:**
- `src/tool/mod.rs` — `Tool` trait, `ToolContext`, `ToolOutput`
- `src/tool/bash.rs` — `BashTool` (shell execution with timeout)
- `src/skill/mod.rs` — `Skill` trait
- `src/skill/builtin.rs` — `BuiltinSkill` (wraps built-in tools)

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

### Object safety

The trait is object-safe thanks to `async_trait` (which boxes the returned
future).  Tools are stored as `Arc<dyn Tool>` throughout Dyson — shared
between skills (which own them) and the agent's flat lookup map.

---

## ToolContext

```rust
pub struct ToolContext {
    pub working_dir: PathBuf,
    pub env: HashMap<String, String>,
    pub cancellation: CancellationToken,
}
```

| Field | Purpose |
|-------|---------|
| `working_dir` | CWD for child processes and path resolution |
| `env` | Environment variables for child processes |
| `cancellation` | Cooperative cancellation (Ctrl-C) |

Created once at agent startup via `ToolContext::from_cwd()`.  Every tool
receives the same context.

---

## ToolOutput

```rust
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub metadata: Option<Value>,
}
```

| Field | Sent to LLM? | Purpose |
|-------|---------------|---------|
| `content` | Yes | Text response shown to the LLM in a `tool_result` block |
| `is_error` | Yes | LLM sees this flag and can retry or adjust |
| `metadata` | No | Internal use: timing, exit codes, byte counts |

**Error distinction:** `ToolOutput { is_error: true }` means the tool ran
but the operation failed (non-zero exit code, file not found).
`Err(DysonError)` means the tool couldn't run at all (can't spawn process,
network down).  Both are converted to `tool_result` blocks for the LLM —
the difference is in logging and metrics.

---

## BashTool

The workhorse tool — the LLM uses it for everything from `ls` to `cargo test`.

```rust
pub struct BashTool {
    pub timeout: Duration,  // default: 120 seconds
}
```

### Input schema

```json
{
  "type": "object",
  "properties": {
    "command": { "type": "string", "description": "The bash command to execute" }
  },
  "required": ["command"]
}
```

### Execution flow

```
1. Extract "command" from JSON input
2. Spawn: bash -c "<command>"
     current_dir = ctx.working_dir
     env = ctx.env
     stdout/stderr piped
3. Wait with timeout (tokio::time::timeout)
4. Combine stdout + stderr
     If both non-empty: separate with "--- stderr ---"
5. Truncate if > 100KB (protects LLM context window)
6. Return ToolOutput
     is_error = exit code != 0
     metadata = { exit_code, stdout_bytes, stderr_bytes }
```

### Output truncation

Commands like `cat huge_file.log` can produce megabytes of output that would
blow the LLM's context window.  BashTool truncates to 100KB and appends a
notice: `"... (output truncated — N bytes omitted, total was M bytes)"`.
The truncation respects UTF-8 char boundaries.

### Timeout handling

If a command exceeds the timeout (default 120s), `wait_with_output()` is
cancelled and an error output is returned: `"Command timed out after 120
seconds"`.  The LLM sees this and can decide to retry with a different
approach.

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

```
Agent starts
  → skill.on_load()          connect to servers, validate prerequisites
  → skill.tools()            agent clones Arc pointers into flat lookup
  → skill.system_prompt()    agent composes the full system prompt

Each tool execution from this skill:
  → tool.run(...)
  → skill.after_tool(name, output)

Agent shuts down:
  → skill.on_unload()        close connections, clean up
```

### Why skills instead of just tools?

Tools are stateless capabilities.  Skills add grouping, lifecycle, and
context:

| Concern | Tool alone | With Skill |
|---------|-----------|------------|
| Grouping | Flat list | Logical bundles (e.g., "all GitHub tools") |
| Setup | None | `on_load()` connects to MCP server |
| Teardown | None | `on_unload()` kills child process |
| LLM context | Tool description only | Skill prompt fragment with usage guidance |
| Post-processing | None | `after_tool()` for logging, metrics |

---

## BuiltinSkill

The default skill that wraps Dyson's built-in tools:

```rust
pub struct BuiltinSkill {
    tools: Vec<Arc<dyn Tool>>,
    system_prompt: String,
}
```

Phase 1 includes only `BashTool`.  Future phases will add:
- `ReadFileTool` — read files with line ranges and binary detection
- `WriteFileTool` — create or overwrite files
- `EditFileTool` — surgical string-replace edits
- `WebSearchTool` — web search via pluggable provider

The system prompt is generated dynamically from the loaded tools — each
tool's name and description are listed so the LLM knows what's available.

---

## Adding a New Tool

1. Create `src/tool/my_tool.rs`
2. Implement the `Tool` trait
3. Add the module declaration to `src/tool/mod.rs`
4. Add the tool to `BuiltinSkill::new()` in `src/skill/builtin.rs`

Example skeleton:

```rust
pub struct MyTool;

#[async_trait]
impl Tool for MyTool {
    fn name(&self) -> &str { "my_tool" }
    fn description(&self) -> &str { "Does something useful" }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "param": { "type": "string", "description": "The input" }
            },
            "required": ["param"]
        })
    }
    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let param = input["param"].as_str()
            .ok_or_else(|| DysonError::tool("my_tool", "missing 'param'"))?;
        Ok(ToolOutput::success(format!("Result: {param}")))
    }
}
```

The agent discovers it automatically via the `Skill` trait — no changes to
the agent loop needed.

---

## Skill Taxonomy (Current and Future)

| Skill | Status | Tools | Source |
|-------|--------|-------|--------|
| `BuiltinSkill` | Implemented | bash (+ future read/write/edit) | Compiled into Dyson |
| `McpSkill` | Planned | Discovered via `tools/list` | MCP server (stdio/SSE) |
| `LocalSkill` | Planned | Defined in SKILL.md files | Local filesystem |

All three will implement the same `Skill` trait.  The agent loop treats them
identically.

---

See also: [Architecture Overview](architecture-overview.md) ·
[Agent Loop](agent-loop.md) · [Sandbox](sandbox.md)
