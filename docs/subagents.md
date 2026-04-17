# Subagents

Subagents are child agents that the parent LLM can invoke as tools.  Each
subagent has its own system prompt, conversation history, and (optionally) a
different model or provider.  When invoked, the subagent runs to completion
and returns its final text as a tool result.

There are three kinds of subagent:

| Kind | Examples | Description |
|------|----------|-------------|
| **Config-driven** | `planner`, `researcher`, `verifier` | Simple single-purpose agents defined by a `SubagentAgentConfig`.  Each gets a filtered set of parent tools and runs a task to completion. |
| **Coder** | `coder` | A built-in subagent that scopes a coding task to a specific directory.  Takes `{ path, task }` input — the path becomes the child's `working_dir`. |
| **Orchestrator** | `security_engineer` | A composable subagent that gets direct tools *and* inner subagent dispatch.  Can run 5+ tool calls in parallel, including dispatching other subagents. |

**Key files:**
- `src/skill/subagent/mod.rs` — `SubagentTool`, `SubagentSkill`, `CaptureOutput`, `FilteredSkill`, `spawn_child`
- `src/skill/subagent/orchestrator.rs` — `OrchestratorTool`, `OrchestratorConfig`
- `src/skill/subagent/coder.rs` — `CoderTool`
- `src/skill/subagent/security_engineer.rs` — `security_engineer_config()`
- `src/skill/subagent/prompts/` — System prompts and protocol injections
- `src/config/mod.rs` — `SubagentAgentConfig`
- `src/tool/security/` — Security tools (`ast_query`, `attack_surface_analyzer`, `exploit_builder`, `taint_trace`)

---

## Built-in Subagents

Dyson ships with these subagents out of the box.  All use the `"default"`
provider (the parent's own model), so they work with zero extra configuration.

### Config-driven Subagents

| Name | Description | Tools | Iterations | Tokens |
|------|-------------|-------|------------|--------|
| `planner` | Breaks down complex tasks into ordered implementation steps.  Read-only — plans but doesn't execute. | `read_file`, `search_files`, `list_files` | 15 | 4096 |
| `researcher` | Deep research with broad access.  Can read code, run commands, and search the web. | `bash`, `read_file`, `search_files`, `list_files`, `web_search` | 20 | 4096 |
| `verifier` | Adversarial validation.  Runs tests, checks edge cases, returns PASS/FAIL/PARTIAL verdict.  Injects a protocol fragment telling the parent when to invoke it. | `bash`, `read_file`, `search_files`, `list_files` | 25 | 8192 |

### Built-in Tools

| Name | Input | Description |
|------|-------|-------------|
| `coder` | `{ path, task }` | Spawns a focused coding agent scoped to a directory.  Gets `bash`, `read_file`, `edit_file`, `search_files`, `list_files`, `bulk_edit`. |
| `security_engineer` | `{ task, context? }` | Orchestrator for security analysis.  Gets AST-aware security tools plus inner subagent dispatch.  See [Orchestrators](#orchestrators) below. |

User-defined subagents in `dyson.json` are prepended before built-ins.

---

## Architecture

### Simple Subagent (config-driven)

```
Parent Agent (depth 0)
  ├── bash, read_file, ...           ← normal tools
  ├── researcher (SubagentTool)      ← spawns child on invocation
  │     └── Child Agent (depth 1)
  │           ├── bash, read_file, search_files, list_files, web_search
  │           └── (runs to completion → returns final text)
  └── coder (CoderTool)
        └── Child Agent (depth 1, working_dir = scoped path)
              ├── bash, read_file, edit_file, search_files, list_files, bulk_edit
              └── (runs to completion → returns final text)
```

### Orchestrator (nested subagents)

```
Parent Agent (depth 0)
  └── security_engineer (OrchestratorTool)
        └── Child Agent (depth 1)
              ├── [direct tools]
              │     ast_query, attack_surface_analyzer, exploit_builder,
              │     taint_trace, dependency_scan,
              │     bash, read_file, search_files, list_files
              └── [inner subagents → depth 2, run in parallel]
                    planner, researcher, coder, verifier
                    └── Inner Child Agents (depth 2, no further subagents)
```

All subagents are **just Tools**.  When the parent LLM calls one:

1. `SubagentTool::run()` (or `OrchestratorTool::run()`) checks the recursion depth
2. Wraps inherited tools in a `FilteredSkill`
3. Constructs a child `Agent` with its own conversation history
4. Runs the child to completion using `CaptureOutput`
5. Returns the captured text as `ToolOutput::success()`

The child inherits the parent's `Arc<dyn Sandbox>` (security cannot be bypassed
by delegation) and `Arc<RwLock<Workspace>>` (shared memory).

---

## Orchestrators

Orchestrators are composable subagents that get both direct tools and inner
subagent dispatch.  They're defined by an `OrchestratorConfig`:

```rust
pub struct OrchestratorConfig {
    pub name: String,              // tool name (e.g., "security_engineer")
    pub description: String,       // shown to the parent LLM
    pub system_prompt: String,     // the orchestrator's personality
    pub direct_tool_names: Vec<String>,  // tool allowlist from parent
    pub max_iterations: usize,
    pub max_tokens: u32,
    pub injects_protocol: Option<String>,  // appended to parent's system prompt
}
```

### How Orchestrators Work

1. **Direct tools** are filtered from the parent's tool set by the allowlist
2. **Inner subagents** (planner, researcher, coder, verifier) are pre-built as
   `SubagentTool`/`CoderTool` instances and passed to the child
3. The child agent gets all of these as a flat tool list
4. Because inner subagents are regular tools with no resource conflicts, the
   dependency analyzer places them in `ExecutionPhase::Parallel` — the child
   can dispatch 5+ tool calls concurrently via `join_all()`

### Adding a New Orchestrator

1. Write a system prompt in `src/skill/subagent/prompts/your_role.md`
2. Create a config function in `src/skill/subagent/your_role.rs`:

```rust
pub fn your_role_config() -> OrchestratorConfig {
    OrchestratorConfig {
        name: "your_role".into(),
        description: "What this orchestrator does.".into(),
        system_prompt: include_str!("prompts/your_role.md").into(),
        direct_tool_names: vec!["bash".into(), "read_file".into(), /* ... */],
        max_iterations: 30,
        max_tokens: 8192,
        injects_protocol: Some(include_str!("prompts/your_role_protocol.md").into()),
    }
}
```

3. Add it to `builtin_orchestrator_configs()` in `src/skill/subagent/mod.rs`:

```rust
pub fn builtin_orchestrator_configs() -> Vec<OrchestratorConfig> {
    vec![
        security_engineer_config(),
        your_role_config(),  // ← add here
    ]
}
```

4. Declare the module in `mod.rs` and `pub use` the config function.

The orchestrator automatically gets inner subagent tools (planner, researcher,
coder, verifier) — no extra wiring needed.

### Security Engineer

The built-in security engineer orchestrator can:

- **Write custom tree-sitter queries** via `ast_query` to trace structural
  patterns (SQL injection sinks, command injection vectors, hardcoded secrets)
  across all 20 supported languages
- **Map attack surfaces** via `attack_surface_analyzer` (HTTP handlers, CLI
  entry points, network listeners, database queries, file I/O, env reads,
  deserialization)
- **Generate exploit PoCs** via `exploit_builder` (payloads, curl commands,
  Nuclei YAML templates, remediation advice)
- **Trace cross-file taint** via `taint_trace` — given a source `file:line`
  (where user input enters) and a sink `file:line` (the dangerous operation),
  returns ranked candidate call chains.  Lossy by design; each hop is a
  hypothesis the agent verifies with `read_file` before filing
- **Scan dependencies** via `dependency_scan` against Google's OSV database
  (every ecosystem OSV tracks; also reads / emits CycloneDX SBOMs)
- **Dispatch subagents in parallel** — researcher for CVE lookups while
  running AST queries, coder for fixes, verifier for validation

The system prompt teaches the agent how to write tree-sitter S-expression
queries and includes p95 common vulnerability patterns as examples.  The agent
constructs and executes its own checks — nothing is hardcoded.

#### Evaluating report quality

Signals to watch when security_engineer reports come back from a live run:

- **Attack Tree depth.**  The system prompt requires every finding to carry
  a root-to-leaf chain to an entry point.  Before `taint_trace`, the agent
  paid 10+ `ast_query` + `read_file` calls per hop and often abandoned the
  chain mid-trace, emitting stub trees with a single hop.  Post-taint_trace,
  expect 2–3 resolved hops on non-trivial findings.  Single-hop trees on
  anything above MEDIUM are a regression signal — the agent isn't using
  `taint_trace` (or is using it with poor source/sink inputs).

- **`resolved_hops / total_hops` per trace.**  Every `taint_trace` output
  includes this ratio in each path header.  A consistent `1/2` or `0/1`
  across a run means the agent is feeding the tool weak hypotheses — the
  normal flow is `ast_query` to discover sources / sinks, then `taint_trace`
  to rank reachability.  Skipping the discovery step produces noise.

- **`UnresolvedCallee` rate.**  The index header reports
  `N unresolved (X%)`.  Baselines from the in-repo smoke
  (`examples/smoke_taint_trace.rs`):
  - 0–2% for most languages (Rust, Go, Python, TypeScript, Swift, C, C++,
    Kotlin, Zig, Ruby, Elixir, Erlang, Java, C#)
  - ~25% for Haskell (typeclass dispatch, operator sections)
  - ~30% for Nix (attribute-path applies, `callPackage` patterns)

  A spike above these baselines on a supported language points at a
  callee-resolution bug in `flatten_callee` — reproduce on a minimal
  fixture and add a regression test to `tests/ast_taint_patterns.rs`.

- **`[TRUNCATED]` in the index header.**  The indexed repo exceeded
  `TAINT_MAX_FILES = 5000`.  Calls in files beyond the cap are invisible;
  any finding that should have crossed into them will miss.  Bump the cap
  in `src/ast/taint/index.rs` if your vectors hit this often.

- **"Checked and cleared" notes.**  The prompt allows these in place of
  low-confidence findings.  Many of them on expected-vulnerable code
  means the agent is dropping findings it could have confirmed — usually
  a sign that `taint_trace` is returning NO_PATH when it shouldn't (wrong
  source line, non-tier-1 language without assignment propagation, or a
  real bug).  Re-run with the source line moved to the exact taint-entry
  point and see if the path materialises.

---

## Depth Budget

```
MAX_SUBAGENT_DEPTH = 3

Parent (depth 0)
  → child (depth 1)
    → inner child (depth 2)
      → cannot spawn further (depth 3 = limit)
```

Subagent tools are excluded from children by construction: `SubagentSkill`
is built in Phase B of `create_skills()`, after Phase A collects
`parent_tools`.  The `MAX_SUBAGENT_DEPTH` check in `spawn_child()` is a
belt-and-suspenders guard.

---

## Parallel Execution

The dependency analyzer (`agent/dependency_analyzer.rs`) only tracks resource
conflicts for `file_read`, `file_write`, and `bash` (git).  Subagent tool calls
return no resource accesses — they fall into the catch-all `_ => {}` branch.
This means multiple subagent calls in a single LLM response are placed in
`ExecutionPhase::Parallel` and run concurrently via `join_all()`.

An orchestrator's child can dispatch 5+ tool calls in one response:

```
ast_query { ... }                    ─┐
attack_surface_analyzer { ... }       │
researcher { task: "Check CVEs" }     ├── all run in parallel
researcher { task: "Audit deps" }     │
planner { task: "Plan review" }      ─┘
```

---

## Configuration

Add custom subagents to your `dyson.json`:

```json
{
  "providers": {
    "claude": {
      "type": "anthropic",
      "models": ["claude-sonnet-4-20250514"],
      "api_key": { "resolver": "insecure_env", "name": "ANTHROPIC_API_KEY" }
    },
    "gpt": {
      "type": "openai",
      "models": ["gpt-4o"],
      "api_key": { "resolver": "insecure_env", "name": "OPENAI_API_KEY" }
    }
  },
  "skills": {
    "builtin": {},
    "subagents": [
      {
        "name": "research_agent",
        "description": "Research specialist using GPT-4o.",
        "system_prompt": "You are a research specialist.",
        "provider": "gpt",
        "max_iterations": 15,
        "tools": ["bash", "web_search", "read_file"]
      }
    ]
  }
}
```

### Config Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `name` | yes | — | Tool name the parent uses to invoke this subagent |
| `description` | yes | — | Shown to the parent LLM so it knows when to delegate |
| `system_prompt` | yes | — | System prompt for the child agent |
| `provider` | yes | — | Key into `providers` map, or `"default"` for the parent's own |
| `model` | no | provider default | Override the model within the provider |
| `max_iterations` | no | 10 | Maximum agent loop iterations |
| `max_tokens` | no | 4096 | Max tokens per LLM response |
| `tools` | no | all parent tools | Tool name filter (see below) |
| `injects_protocol` | no | — | Markdown fragment appended to the parent's system prompt |

### Tool Inheritance

Subagents inherit the parent's loaded tools (builtins, MCP, local skills) via
`Arc<dyn Tool>` clones — no duplication, no reconnecting to MCP servers.

- **`tools` omitted**: subagent gets all parent tools (minus subagent tools)
- **`tools: ["bash", "read_file"]`**: only those named tools are visible
- Unknown tool names are silently ignored

### Protocol Injection

Any subagent (config-driven or orchestrator) can inject a protocol fragment
into the parent's system prompt via `injects_protocol`.  This tells the
parent *when* to invoke the subagent:

```json
{
  "name": "verifier",
  "injects_protocol": "\n## Verification Protocol\nAfter 3+ file changes, invoke verifier.\n"
}
```

The fragment is appended after the subagent listing in the parent's prompt.

---

## Components

| Component | Location | Purpose |
|-----------|----------|---------|
| `SubagentTool` | `subagent/mod.rs` | `Tool` impl for config-driven subagents.  Input: `{ task, context? }` |
| `CoderTool` | `subagent/coder.rs` | `Tool` impl scoped to a directory.  Input: `{ path, task }` |
| `OrchestratorTool` | `subagent/orchestrator.rs` | Generic composable orchestrator.  Takes `OrchestratorConfig`, gets direct tools + inner subagents |
| `OrchestratorConfig` | `subagent/orchestrator.rs` | Data struct defining an orchestrator's identity, tools, and limits |
| `SubagentSkill` | `subagent/mod.rs` | Bundles all subagent tools into a `Skill` with system prompt |
| `FilteredSkill` | `subagent/mod.rs` | Lightweight `Skill` wrapper around pre-loaded parent tools |
| `CaptureOutput` | `subagent/mod.rs` | Collects child's `text_delta` events; tool events logged but discarded |
| `spawn_child()` | `subagent/mod.rs` | Shared lifecycle: depth check → build agent → run → capture → return |

---

## Testing

### Unit Tests (`src/skill/subagent/tests.rs`)

- **CaptureOutput**: text collection, tool event handling, flush, file sends
- **SubagentTool**: name/schema metadata, depth limit, missing task error
- **CoderTool**: path scoping, tool filtering, depth limit, missing path/task
- **OrchestratorTool**: config-driven name/description, tool filtering by
  allowlist, depth limit, child execution, custom config composability
- **SubagentSkill**: system prompt generation, unknown provider handling, protocol injection
- **filter_tools**: all/named/unknown/empty filter cases
- **builtin configs**: planner/researcher/verifier configs, orchestrator configs

### Running Tests

```bash
# All tests
cargo test

# Subagent tests only
cargo test subagent

# Security tool tests
cargo test security
```
