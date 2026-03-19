# Sandbox

The sandbox is the security gate between the LLM and tool execution. Every
tool call passes through the sandbox before running. The sandbox can allow,
deny, or redirect calls — giving you a hook to enforce policies, route tools
to containers, or audit everything the agent does.

**Key files:**
- `src/sandbox/mod.rs` — `Sandbox` trait, `SandboxDecision`
- `src/sandbox/no_sandbox.rs` — `DangerousNoSandbox` (passthrough)
- `src/sandbox/docker.rs` — `DockerSandbox` (route bash to a container)
- `src/sandbox/composite.rs` — `CompositeSandbox` (chain multiple sandboxes)

---

## Sandbox Trait

```rust
#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn check(
        &self,
        tool_name: &str,
        input: &Value,
        ctx: &ToolContext,
    ) -> Result<SandboxDecision>;

    async fn after(
        &self,
        tool_name: &str,
        input: &Value,
        output: &mut ToolOutput,
    ) -> Result<()> {
        Ok(())
    }
}
```

| Method | When called | Purpose |
|--------|-------------|---------|
| `check()` | Before every tool call | Decide: Allow, Deny, or Redirect |
| `after()` | After tool executes (success only) | Post-process output: redact, audit, truncate |

### SandboxDecision

```rust
pub enum SandboxDecision {
    Allow { input: Value },
    Deny { reason: String },
    Redirect { tool_name: String, input: Value },
}
```

| Variant | What happens | LLM sees |
|---------|-------------|----------|
| `Allow { input }` | Tool runs with (possibly rewritten) input | Normal tool result |
| `Deny { reason }` | Tool does NOT run | Error tool_result with deny reason |
| `Redirect { tool_name, input }` | A *different* tool runs | Normal tool result (LLM doesn't know) |

---

## Implementations

### DangerousNoSandbox

Passthrough — allows every tool call unchanged. Selected via
`--dangerous-no-sandbox`. The name is intentionally alarming.

```rust
impl Sandbox for DangerousNoSandbox {
    async fn check(&self, _, input: &Value, _) -> Result<SandboxDecision> {
        Ok(SandboxDecision::Allow { input: input.clone() })
    }
}
```

### DockerSandbox

Routes bash commands through a Docker container. The LLM thinks it's
running commands on the host — they actually run inside an isolated
container.

```rust
let sandbox = DockerSandbox::new("dyson-sandbox");
```

What happens:

```
LLM says:  bash {"command": "cat /etc/passwd"}

check() rewrites to:
  docker exec dyson-sandbox bash -c 'cat /etc/passwd'

BashTool runs the docker exec on your host.
Output comes from inside the container — not your machine.
```

Start the container:
```bash
docker run -d --name dyson-sandbox \
  -v $(pwd):/workspace \
  -w /workspace \
  --network none \
  --memory 512m \
  ubuntu:24.04 sleep infinity
```

Security properties:
- Filesystem isolation — can't read host files outside mounts
- Process isolation — can't see or kill host processes
- Network isolation (with `--network none`)
- Resource limits (with `--memory`, `--cpus`)

Non-bash tools (MCP, web_search, etc.) pass through unchanged — they're
network I/O, not host access.

### CompositeSandbox

Chains multiple sandboxes into a pipeline. Each sandbox gets a turn.
First `Deny` wins. `Allow` passes the (possibly rewritten) input to the
next sandbox.

```rust
let sandbox = CompositeSandbox::new(vec![
    Box::new(AuditSandbox::new("audit.log")),
    Box::new(FileSandbox::block(vec!["/etc/shadow", "/root"])),
    Box::new(DockerSandbox::new("dyson-sandbox")),
]);
```

---

## How Composition Works

```
CompositeSandbox([AuditSandbox, FileSandbox, DockerSandbox])

Tool call: bash {"command": "cat /etc/shadow"}
  │
  ▼
AuditSandbox.check("bash", {"command": "cat /etc/shadow"})
  → Allow { input unchanged }     ← logs it, doesn't block
  │
  ▼
FileSandbox.check("bash", {"command": "cat /etc/shadow"})
  → Deny { "/etc/shadow is restricted" }
  │
  ▼
STOP — first Deny wins.  DockerSandbox never runs.
```

Another example — rewrite chaining:

```
CompositeSandbox([AuditSandbox, DockerSandbox])

Tool call: bash {"command": "ls"}
  │
  ▼
AuditSandbox.check("bash", {"command": "ls"})
  → Allow { input unchanged }
  │
  ▼
DockerSandbox.check("bash", {"command": "ls"})
  → Allow { input: {"command": "docker exec sandbox bash -c 'ls'"} }
  │
  ▼
Final: Allow with rewritten input.
BashTool runs: docker exec sandbox bash -c 'ls'
```

### Pipeline rules

| Event | Behavior |
|-------|----------|
| `Deny` | Stop immediately, return denial |
| `Redirect` | Stop immediately, return redirect |
| `Allow { input }` | Pass (possibly rewritten) input to next sandbox |
| All sandboxes allow | Return final Allow with accumulated rewrites |
| `after()` | All sandboxes run in order, each can mutate output |

### Ordering guidelines

| Position | What to put here | Why |
|----------|-----------------|-----|
| First | Sandboxes that DENY (blacklists, path blocks) | Fail fast |
| Middle | Sandboxes that REWRITE (Docker, path remapping) | Transform before execution |
| First or last | Sandboxes that OBSERVE (audit) | Log original input or final rewritten input |

---

## How Sandboxes Know What To Do

A sandbox receives `(tool_name: &str, input: &Value)` — just a string and
a JSON blob. It pattern-matches on the tool name:

```rust
match tool_name {
    "bash" => /* rewrite for docker */,
    "read_file" => /* check path restrictions */,
    "web_fetch" => /* check URL whitelist */,
    _ => /* don't recognize it, pass through */,
}
```

Sandboxes that don't recognize a tool name return
`Allow { input: input.clone() }` — pass through, let the next sandbox
in the chain decide. This means each sandbox is small and focused:

| Sandbox | Cares about | Ignores |
|---------|------------|---------|
| DockerSandbox | `"bash"` | everything else |
| FileSandbox | `"read_file"`, `"write_file"` | everything else |
| NetworkSandbox | `"web_fetch"`, `"web_search"` | everything else |
| AuditSandbox | everything | nothing (logs all) |

Composing them gives you full coverage without any single sandbox
needing to know about all tools.

---

## Future Sandbox Implementations

| Sandbox | `check()` behavior | `after()` behavior |
|---------|-------------------|-------------------|
| `BlacklistSandbox` | Deny commands matching regex patterns | None |
| `FileSandbox` | Deny/rewrite file paths outside allowed dirs | None |
| `NetworkSandbox` | Deny URLs not in whitelist | None |
| `S3Sandbox` | Redirect file read/write to S3 | None |
| `AuditSandbox` | Allow everything | Log all calls to a file |
| `RateLimitSandbox` | Deny after N calls per minute | None |

---

See also: [Architecture Overview](architecture-overview.md) ·
[Agent Loop](agent-loop.md) · [Tools & Skills](tools-and-skills.md)
