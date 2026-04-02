# Tool Execution Pipeline

When the LLM requests tool calls, Dyson doesn't just fire them all in parallel.
A four-stage pipeline checks limits, analyzes dependencies, executes in the
correct order, and formats results for LLM consumption.

**Key files:**
- `src/agent/tool_limiter.rs` — `ToolLimiter` (per-turn rate limiting and cooldown)
- `src/agent/dependency_analyzer.rs` — `DependencyAnalyzer` (resource conflict detection, phase grouping)
- `src/agent/result_formatter.rs` — `ResultFormatter`, `FormattedResult` (structured LLM-optimized output)
- `src/tool_hooks.rs` — `ToolHook` trait, `ToolHookEvent`, `dispatch_hooks()` (lifecycle hooks)
- `src/agent/mod.rs` — Pipeline integration in the agent loop

---

## Pipeline Overview

```
LLM requests tool calls
       │
       ▼
┌─────────────────────────┐
│  1. LIMIT               │  ToolLimiter.check()
│     Per-turn count cap   │  Rejects calls over the limit
│     Cooldown enforcement │  with an error tool_result
└───────────┬─────────────┘
            │ allowed calls
            ▼
┌─────────────────────────┐
│  2. ANALYZE             │  DependencyAnalyzer.analyze()
│     Resource extraction  │  Groups calls into phases:
│     Conflict detection   │    Parallel (independent)
│     Topological layering │    Sequential (dependent)
└───────────┬─────────────┘
            │ Vec<ExecutionPhase>
            ▼
┌─────────────────────────┐
│  3. EXECUTE             │  Per phase:
│     Parallel phases:     │    join_all(futures)
│     Sequential phases:   │    one-by-one in order
│     Sandbox gating       │    sandbox.check() before each
└───────────┬─────────────┘
            │ ToolOutput per call
            ▼
┌─────────────────────────┐
│  4. FORMAT              │  ResultFormatter.format()
│     Structured summary   │  → FormattedResult
│     Key line extraction  │  → to_llm_message()
│     Truncation markers   │  → tool_result message
└─────────────────────────┘
```

After all phases complete, `limiter.reset_turn()` clears per-turn counters
for the next iteration.

---

## Stage 1: ToolLimiter

Prevents runaway tool use by enforcing two constraints:

| Constraint | Default | Purpose |
|-----------|---------|---------|
| Per-turn limit | 50 calls per tool | Prevents infinite tool loops |
| Cooldown | 1 second (0 for agent mode) | Rate-limits across turns |

```rust
pub struct ToolLimiter {
    per_turn_limit: usize,
    cooldown: Duration,
    turn_counts: HashMap<String, usize>,
    last_call: HashMap<String, Instant>,
}
```

### How it works

`check(tool_name)` does two things atomically:
1. Checks if the per-turn count for this tool has reached the limit
2. Checks if enough time has elapsed since the last call (cooldown)

If either check fails, it returns `Err(DysonError::Tool { .. })`.  On success,
it increments the counter and records the timestamp.

### Agent vs external mode

`ToolLimiter::for_agent()` creates a limiter with zero cooldown.  Within a
single agent turn, multiple calls to the same tool (e.g., reading 10 files)
should not be delayed by cooldown — that's what per-turn limits prevent.
Cooldown is useful for rate-limiting external callers or cross-turn bursts.

### Turn reset

`reset_turn()` clears per-turn counters but preserves cooldown timestamps.
Called at the end of each agent loop iteration.

---

## Stage 2: DependencyAnalyzer

Analyzes a batch of tool calls to detect resource conflicts and groups them
into execution phases.

```rust
pub struct DependencyAnalyzer;

pub enum ExecutionPhase {
    Parallel(Vec<usize>),    // can run concurrently
    Sequential(Vec<usize>),  // must run in order
}
```

### Resource model

Each tool call is analyzed for the resources it accesses:

| Tool | Resource | Access kind |
|------|----------|-------------|
| `file_read` | `File(path)` | Read |
| `file_write` | `File(path)` | Write |
| `bash` (git commands) | `Git` | Read or Write (classified per subcommand) |
| Other bash / unknown | (none) | Treated as independent |

Git commands are classified by subcommand:
- **Write**: `git add`, `git commit`, `git push`, `git checkout`, `git reset`,
  `git merge`, `git rebase`, `git pull`, `git stash`, `git rm`, `git mv`, `git tag`
- **Read**: `git status`, `git log`, `git diff`, `git show`, `git branch`, `git remote`
- **Unknown git**: treated as Write (safe default)

### Dependency rules

A dependency exists between two calls when they access the same resource and
at least one access is a Write:

| Earlier → Later | Dependency? | Reason |
|-----------------|-------------|--------|
| Read → Read | No | Both only observe |
| Read → Write | Yes | RAW (read-after-write prevention) |
| Write → Read | Yes | WAR (must see the write's result) |
| Write → Write | Yes | WAW (ordering matters) |

### Phase construction

1. **Extract resources** for each call
2. **Build dependency edges** — for each pair (i, j) where i < j, check if j
   depends on i
3. **Topological layering** — assign each call a depth (0 = no dependencies,
   1 = depends on depth-0 calls, etc.)
4. **Group by depth** — calls at the same depth form a phase
5. **Classify phases** — if calls within a phase conflict with each other,
   mark Sequential; otherwise mark Parallel

### Example

```
Call 0: file_write("config.json")    depth 0
Call 1: bash("echo hello")           depth 0 (independent)
Call 2: file_read("config.json")     depth 1 (depends on call 0)
```

Result: `[Parallel([0, 1]), Sequential([2])]`
- Phase 1: write config.json and echo run concurrently
- Phase 2: read config.json runs after phase 1 completes

---

## Stage 3: Execution

The agent loop iterates through phases in order:

```rust
for phase in phases {
    match phase {
        ExecutionPhase::Parallel(indices) => {
            let futs = indices.iter()
                .map(|&idx| self.execute_tool_call_timed(call));
            futures::future::join_all(futs).await;
        }
        ExecutionPhase::Sequential(indices) => {
            for &idx in &indices {
                self.execute_tool_call_timed(call).await;
            }
        }
    }
}
```

Each individual tool call still goes through the sandbox (`sandbox.check()`)
before execution — dependency analysis doesn't bypass security.

---

## Stage 4: ResultFormatter

Formats raw `ToolOutput` into structured, LLM-optimized results.

```rust
pub struct FormattedResult {
    pub summary: String,           // "bash: `cargo build` completed in 342ms (exit 0)"
    pub output: String,            // the actual stdout/stderr content
    pub key_lines: Vec<String>,    // errors, warnings, compilation messages
    pub exit_code: Option<i32>,    // inferred for bash tools
    pub truncated: bool,           // output exceeded 30KB threshold
    pub full_output_available: bool,
}
```

### Tool-specific formatting

| Tool | Summary format | Key lines | Exit code |
|------|---------------|-----------|-----------|
| `bash` | `` bash: `<cmd>` completed/failed in Nms (exit X) `` | Error/warning markers | Inferred (0, 1, or 127) |
| `file_read` | `file_read: <path> (N bytes, Nms)` | (none) | (none) |
| `file_write` | `file_write: <path> — <status> (Nms)` | (none) | (none) |
| Other | `<name>: ok/error (Nms)` | Error/warning markers | (none) |

### Key line extraction

Lines containing any of these markers are extracted (up to 20):
`Compiling`, `Finished`, `error`, `warning`, `Error`, `Warning`,
`FAILED`, `PASSED`, `panic`, `thread '`

### LLM message construction

`FormattedResult.to_llm_message()` combines the summary, output content, and
truncation notice into the string sent as the `tool_result` content block.
This ensures the LLM sees both the metadata summary and the actual command
output.

### Truncation

Outputs exceeding 30,000 characters are marked as truncated.  The
`full_output_available` flag indicates the raw output can be retrieved
separately if needed.

---

## Tool Hooks

Lifecycle hooks that run before and after each tool execution.

```rust
pub trait ToolHook: Send + Sync {
    fn on_event(&self, event: &ToolHookEvent) -> HookDecision;
}

pub enum ToolHookEvent<'a> {
    PreToolUse { call: &'a ToolCall },
    PostToolUse { output: &'a ToolOutput, duration: Duration },
    PostToolUseFailure { error: &'a DysonError },
}

pub enum HookDecision {
    Allow,
    Block { reason: String },
    Modify { input: serde_json::Value },
}
```

### Dispatch rules

| Event | Effect of decisions |
|-------|--------------------|
| `PreToolUse` | First `Block` or `Modify` wins; `Allow` continues to next hook |
| `PostToolUse` | Observational only — decisions ignored, all hooks called |
| `PostToolUseFailure` | Observational only — decisions ignored, all hooks called |

### Example hooks

**Block dangerous commands:**
```rust
impl ToolHook for BlockDangerousHook {
    fn on_event(&self, event: &ToolHookEvent) -> HookDecision {
        if let ToolHookEvent::PreToolUse { call } = event {
            if let Some(cmd) = call.input.get("command").and_then(|v| v.as_str()) {
                if cmd.contains("rm -rf") {
                    return HookDecision::Block {
                        reason: "dangerous command blocked".into(),
                    };
                }
            }
        }
        HookDecision::Allow
    }
}
```

**Add timeout to bash calls:**
```rust
impl ToolHook for AddTimeoutHook {
    fn on_event(&self, event: &ToolHookEvent) -> HookDecision {
        if let ToolHookEvent::PreToolUse { call } = event {
            if call.name == "bash" {
                let mut input = call.input.clone();
                input["timeout"] = json!(30);
                return HookDecision::Modify { input };
            }
        }
        HookDecision::Allow
    }
}
```

---

See also: [Agent Loop](agent-loop.md) ·
[Tools & Skills](tools-and-skills.md) ·
[Architecture Overview](architecture-overview.md) ·
[Sandbox](sandbox.md)
