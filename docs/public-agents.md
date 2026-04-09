# Public Agents

A **public agent** is a Dyson agent exposed to untrusted users — people who
didn't configure it and shouldn't have access to the host machine.

A private agent (the default) has full tools and full workspace access.
A public agent is the same agent architecture — same workspace trait, same
tool system — but wrapped in a `ChannelWorkspace` that enforces a
default-deny write policy, and built with a filtered skill set that
excludes filesystem, shell, MCP, and subagent tools.

The core problem: an LLM agent with shell access is powerful for its owner
but dangerous when strangers can talk to it. Prompt injection in a private
chat is the owner's risk to manage. Prompt injection in a group chat means
anyone can try to make the agent `rm -rf /`, read `/etc/shadow`, or hit the
cloud metadata endpoint. Public agents solve this by removing dangerous
tools entirely, restricting workspace writes to a whitelist, and enforcing
SSRF protection on web access.

**Current support:** Telegram group chats (automatic — just add the bot to
a group). The public/private decision is made at the controller level via
the `AgentMode` enum. Any future controller (HTTP API, Discord, Slack)
passes `AgentMode::Public` to the same `build_agent()` function.

**Key files:**
- `src/controller/mod.rs` — `AgentMode`, `build_agent()`, `build_public_agent()`
- `src/workspace/channel.rs` — `ChannelWorkspace` (default-deny write wrapper)
- `src/workspace/mod.rs` — `create_channel_workspace()` (factory)
- `src/controller/telegram/mod.rs` — maps `is_group` to `AgentMode`
- `src/sandbox/policy_sandbox.rs` — SSRF protection for `web_fetch`

---

## What a Public Agent Can Do

| Capability | Private | Public |
|-----------|---------|--------|
| Web search | Yes | Yes |
| Fetch web pages | Yes | Yes (SSRF-protected) |
| Shell commands | Yes | **No** |
| Read/write files | Yes | **No** |
| Identity (SOUL.md, IDENTITY.md) | Read/write | **Read-only** (symlinked) |
| Memory (MEMORY.md, USER.md) | Read/write | Read/write (per-channel) |
| Journals (memory/*.md) | Read/write | Read/write (per-channel) |
| Memory search (FTS5) | Yes | Yes (per-channel) |
| Skills / MCP / subagents | Yes | **No** |
| Dreams | Yes | Yes (per-channel) |

---

## Architecture

### ChannelWorkspace

`ChannelWorkspace` wraps any `Box<dyn Workspace>` and enforces a
default-deny write policy. Only explicitly whitelisted keys are writable
— everything else is silently dropped.

```rust
ChannelWorkspace::new(Box::new(inner))
    .allow("MEMORY.md")
    .allow("USER.md")
    .allow_prefix("memory/")
```

This is a decorator over the `Workspace` trait:
- **Reads** delegate straight through to the inner workspace.
- **Writes** (`set`, `append`) are forwarded only if the key is allowed.
- **`skill_dirs()`** returns empty — public agents don't load skills.
- **`programs_dir()`** returns `None` — no filesystem project directory.

The Workspace trait stays clean — no policy methods. The write restriction
is an implementation detail of the `ChannelWorkspace` wrapper.

### Per-Channel Workspace Directory

Each public agent gets its own workspace on disk at
`~/.dyson/channels/{channel_id}/`, completely independent of the
operator's main workspace at `~/.dyson/`. The only connection is symlinks
for identity files:

```
~/.dyson/                           # Operator workspace (private agent)
├── SOUL.md
├── IDENTITY.md
├── MEMORY.md
├── memory/
├── memory.db
└── channels/                       # Channel workspaces (public agents)
    ├── -1001234567890/
    │   ├── SOUL.md → ../../SOUL.md         # Symlink — reads propagate
    │   ├── IDENTITY.md → ../../IDENTITY.md # Symlink — reads propagate
    │   ├── MEMORY.md                       # Channel's own memory
    │   ├── USER.md                         # Channel's own user profile
    │   ├── memory/                         # Channel journals
    │   └── memory.db                       # Channel FTS5 index
    └── -1009876543210/
        └── ...
```

Identity symlinks are created on first use. Changes to the operator's
SOUL.md or IDENTITY.md propagate automatically — the symlinks are followed
at read time. On workspace reload (config change), the channel workspace
re-reads through the symlinks and picks up the new content.

### Agent Construction

```text
build_agent(settings, prompt, mode, client, registry, channel_id)
  │
  ├── AgentMode::Private
  │     → create_workspace()           # loads ~/.dyson/
  │     → all skills (builtin + MCP + local + subagent)
  │     → full workspace
  │
  └── AgentMode::Public
        → create_channel_workspace()   # loads ~/.dyson/channels/{id}/
        │   → OpenClawWorkspace::load()
        │   → wrapped in ChannelWorkspace (default-deny writes)
        → filtered skills (workspace memory + web only)
        → sandbox always enforced
```

Controllers just declare the mode and provide a channel ID.

---

## Security Model

| Concern | Enforcement | Location |
|---------|-------------|----------|
| Tool restriction | `BuiltinSkill::new_filtered()` with `PUBLIC_AGENT_TOOLS` allowlist | `build_public_agent()` |
| No bash/file access | Tools not in registry — LLM cannot call them | `skill/builtin.rs` |
| Default-deny writes | `ChannelWorkspace` only forwards writes for whitelisted keys | `channel.rs` |
| Workspace isolation | Separate directory per channel, independent of operator workspace | `create_channel_workspace()` |
| Identity propagation | SOUL.md/IDENTITY.md are symlinks to operator workspace | `create_channel_workspace()` |
| SSRF protection | `PolicySandbox` blocks internal/private IPs for `web_fetch` | `policy_sandbox.rs` |
| Sandbox always active | `create_sandbox(config, false)` — ignores `--dangerous-no-sandbox` | `build_public_agent()` |
| Groups auto-allowed | Group chats bypass `allowed_chat_ids` | Telegram controller |

### Write Policy

The `ChannelWorkspace` uses a whitelist model:

**Writable** (explicitly allowed):
- `MEMORY.md` — channel memory
- `USER.md` — channel user profile
- `memory/*` — daily journals (prefix match)

**Protected** (everything else, including):
- `SOUL.md`, `IDENTITY.md` — symlinked identity
- `AGENTS.md`, `HEARTBEAT.md` — operator config
- Any new file the agent tries to create

### SSRF Protection

The `PolicySandbox` blocks `web_fetch` requests to internal networks:
- Loopback (`127.0.0.0/8`, `::1`, `localhost`)
- Private (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`)
- Link-local (`169.254.0.0/16` — includes cloud metadata), `fe80::/10`
- IPv6 ULA (`fc00::/7`), multicast, reserved hostnames

Hostnames are resolved via DNS before checking. The SSRF check lives in the
sandbox layer and applies to all agents (public and private).

---

## Adding Public Agent Support to a New Controller

Pass `AgentMode::Public` and a channel ID to `build_agent()`. The
controller module handles workspace creation, tool filtering, and sandbox
enforcement. The controller just decides *when* to use public mode:

- **Telegram**: `msg.chat.is_group()` → `AgentMode::Public`, `chat_id` as channel ID
- **HTTP API**: config flag, endpoint path, or auth level
- **Discord**: public channels vs DMs

---

## Configuration

### Telegram Privacy Mode

Telegram bots have **privacy mode enabled by default** — the bot only
receives `/commands` and replies in groups. To let users interact via
`@botname` mentions, disable privacy mode:

1. `@BotFather` → `/mybots` → select bot → **Bot Settings** → **Group Privacy** → **Turn off**
2. **Remove and re-add** the bot to existing groups

Even with privacy mode off, Dyson only processes messages directed at the
bot in groups — @mentions, replies, and `/commands`.

### Chat ID Allowlists

Group chats are automatically allowed (they run as public agents). Private
chats require `allowed_chat_ids`:

```json
{
  "controllers": [
    {
      "type": "telegram",
      "bot_token": "...",
      "allowed_chat_ids": [123456789]
    }
  ]
}
```

### SSRF Policy Override

```json
{
  "sandbox": {
    "tool_policies": {
      "web_fetch": { "network": "deny" }
    }
  }
}
```

Setting `network: "deny"` disables `web_fetch` entirely. The SSRF check
cannot be disabled — it's always applied when network access is allowed.

---

## Reload Behavior

When config or workspace files change, all agents are rebuilt. Public agents
stay restricted — rebuilt with `AgentMode::Public` and a fresh channel
workspace that re-reads identity through the symlinks.
