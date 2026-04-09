# Public Agents

A **public agent** is a Dyson agent exposed to untrusted users — people who
didn't configure it and shouldn't have access to the host machine. A private
agent (the default) has full tools: bash, file I/O, workspace, skills. A
public agent is restricted to workspace memory and web research tools, with
a per-channel workspace for persistent memory.

The core problem: an LLM agent with shell access is powerful for its owner
but dangerous when strangers can talk to it. Prompt injection in a private
chat is the owner's risk to manage. Prompt injection in a group chat or
public API means anyone can try to make the agent `rm -rf /`, read
`/etc/shadow`, or hit the cloud metadata endpoint. Public agents solve this
by removing the dangerous tools entirely, restricting workspace writes to
memory files, and enforcing SSRF protection on what remains.

**Current support:** Telegram group chats (automatic — just add the bot to
a group). The public/private decision is made at the controller level via
the `AgentMode` enum. Any future controller (HTTP API, Discord, Slack)
passes `AgentMode::Public` to the same `build_agent()` function.

**Key files:**
- `src/controller/mod.rs` — `AgentMode` enum, `build_agent()`, `build_public_agent()`, `ClientRegistry`
- `src/controller/telegram/mod.rs` — `get_or_create_entry()` maps `is_group` to `AgentMode`
- `src/controller/telegram/types.rs` — `ChatType` enum, `Chat::is_group()`
- `src/workspace/mod.rs` — `create_channel_workspace()` (per-channel workspace factory)
- `src/sandbox/policy_sandbox.rs` — `check_web_fetch()`, `check_url_not_internal()` (SSRF protection)
- `src/sandbox/policy.rs` — `web_fetch` default policy entry

---

## What a Public Agent Can Do

| Capability | Private agent | Public agent |
|-----------|---------------|--------------|
| Web search | Yes | Yes |
| Fetch web pages | Yes | Yes (SSRF-protected) |
| Run shell commands | Yes | **No** |
| Read/write files | Yes | **No** |
| Identity (SOUL.md, IDENTITY.md) | Yes (read/write) | **Read-only** (symlinked from main workspace) |
| Workspace memory (MEMORY.md, USER.md) | Yes | Yes (per-channel) |
| Journals (memory/*.md) | Yes | Yes (per-channel) |
| Memory search (FTS5) | Yes | Yes (per-channel) |
| Load/create skills | Yes | **No** |
| MCP tools | Yes | **No** |
| Subagents | Yes | **No** |
| Dreams (background cognition) | Yes | Yes (per-channel workspace) |

A public agent is a web research assistant with persistent channel-scoped
memory. It can answer questions using search and page fetching, remember
things across conversations via its workspace, and dream between turns —
but it cannot touch the host system.

---

## How It Works

### Per-Channel Workspaces

Each public agent (e.g. each Telegram group chat) gets its own workspace
directory at `~/.dyson/channels/{chat_id}/`. This directory contains:

- **SOUL.md** — symlink to the main workspace's SOUL.md (read-only)
- **IDENTITY.md** — symlink to the main workspace's IDENTITY.md (read-only)
- **MEMORY.md** — channel-specific memory (writable)
- **USER.md** — channel-specific user profile (writable)
- **memory/** — daily journals (writable)
- **memory.db** — FTS5 index for memory search

Identity files are symlinks so that changes to the operator's main workspace
(e.g. personality tweaks in SOUL.md) automatically propagate to all channel
workspaces on reload. The symlinks are created on first use and left in
place on subsequent loads.

### Agent Construction

The `AgentMode` enum and `build_agent()` function in `src/controller/mod.rs`
are the single point of control:

```rust
pub enum AgentMode {
    /// Full-featured agent for trusted users.
    Private,
    /// Per-channel workspace agent for untrusted users.
    Public,
}

pub async fn build_agent(
    settings: &Settings,
    controller_prompt: Option<&str>,
    mode: AgentMode,
    client: RateLimitedHandle<Box<dyn LlmClient>>,
    registry: &mut ClientRegistry,
    channel_id: Option<&str>,
) -> Result<Agent>
```

Controllers don't configure tools, sandbox, or workspace themselves — they
just declare the mode and provide a channel ID, and `build_agent` handles
the rest.

### Telegram Integration

The Telegram controller detects group chats via the `ChatType` enum
deserialized from the Telegram Bot API's `Chat` object. Group chats are
automatically allowed without being in `allowed_chat_ids` — they always
run as public agents, so they're safe without explicit whitelisting.

```text
Message arrives from Telegram
  │
  ├── ChatType::Private     →  build_agent(..., AgentMode::Private, None)
  └── ChatType::Group       →  build_agent(..., AgentMode::Public, Some(chat_id))
          or Supergroup
```

Each chat gets its own agent instance, conversation history, and (for
public agents) its own workspace directory. All agents share the same
LLM client and rate-limit window via `ClientRegistry`.

---

## Security Model

| Concern | Enforcement | Location |
|---------|-------------|----------|
| Tool restriction | `BuiltinSkill::new_filtered()` with `PUBLIC_AGENT_TOOLS` allowlist | `build_public_agent()` |
| No bash/file access | Tools not in registry — LLM cannot call them | `skill/builtin.rs` |
| Read-only identity | SOUL.md/IDENTITY.md symlinked from main workspace; writes blocked by `read_only_files` | `build_public_agent()`, `workspace_update.rs` |
| Per-channel isolation | Separate workspace directory per channel ID | `create_channel_workspace()` |
| No dangerous workspace writes | `ToolContext.read_only_files` blocks writes to SOUL.md, IDENTITY.md, AGENTS.md, HEARTBEAT.md | `workspace_update.rs` |
| Dreams (per-channel) | Dream thread uses channel workspace, not main workspace | `build_public_agent()` |
| SSRF protection | `PolicySandbox` blocks internal/private IPs for `web_fetch` | `policy_sandbox.rs` |
| Sandbox always active | `create_sandbox(config, false)` — hardcoded, ignores `--dangerous-no-sandbox` | `build_public_agent()` |
| Groups auto-allowed | Group chats bypass `allowed_chat_ids` — they're always public and safe | Telegram controller |

### SSRF Protection

The `web_fetch` tool lets the LLM fetch arbitrary URLs. In a public context,
this creates an SSRF risk — an attacker could prompt-inject the LLM into
fetching internal services (databases, cloud metadata, admin panels).

The `PolicySandbox` blocks `web_fetch` requests to:
- **Loopback**: `127.0.0.0/8`, `::1`, `localhost`, `*.localhost`
- **Private**: `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`
- **Link-local**: `169.254.0.0/16` (includes cloud metadata at `169.254.169.254`), `fe80::/10`
- **IPv6 ULA**: `fc00::/7`
- **Multicast**: `224.0.0.0/4`, `ff00::/8`
- **Reserved hostnames**: `*.internal`, `metadata.google.internal`
- **IPv4-mapped IPv6**: `::ffff:10.x.x.x`, `::ffff:192.168.x.x`, etc.

Hostnames are resolved via DNS before checking — this catches DNS entries
that point to internal IPs (e.g., `internal.company.com` → `10.0.0.5`).

The SSRF check lives in the sandbox layer, not in the `web_fetch` tool.
This means it's enforced regardless of how `web_fetch` is called and cannot
be bypassed by prompt injection. It also applies to all agents — private
agents get SSRF protection too, not just public ones.

### Read-Only Files

Public agents can read but not write to identity files. The
`ToolContext.read_only_files` list is set by `build_public_agent()` and
checked by `workspace_update` before every write. Protected files:

- `SOUL.md` — agent personality (symlinked from main workspace)
- `IDENTITY.md` — agent identity (symlinked from main workspace)
- `AGENTS.md` — operating procedures
- `HEARTBEAT.md` — periodic task checklist

The agent can freely write to `MEMORY.md`, `USER.md`, and `memory/*.md`
journal files. This gives it persistent memory while protecting the
operator's identity configuration from prompt injection.

---

## Adding Public Agent Support to a New Controller

Pass `AgentMode::Public` and a channel ID to `build_agent()`. That's it.
The controller module handles tool filtering, workspace creation, sandbox
enforcement, and read-only protection. The controller just needs to decide
*when* to use `AgentMode::Public` and provide a unique channel identifier:

- **Telegram**: `msg.chat.is_group()` maps to `AgentMode::Public`, `chat_id` as channel ID.
- **HTTP API**: could be a config flag, endpoint path, or auth level.
- **Discord**: public channels vs DMs.

---

## Configuration

### Telegram Privacy Mode

Telegram bots have **privacy mode enabled by default**, which means the bot
only receives `/commands` and replies in groups — not regular messages or
@mentions.  To let users interact with the bot via `@botname` mentions,
**disable privacy mode**:

1. Open `@BotFather` on Telegram.
2. `/mybots` → select your bot → **Bot Settings** → **Group Privacy** → **Turn off**.
3. **Remove and re-add** the bot to existing groups (required for the change
   to take effect in groups the bot was already in).

Even with privacy mode off, Dyson **only processes messages directed at the
bot** in groups — @mentions, replies to the bot, and `/commands`.  Other
group messages are silently ignored, so the bot doesn't burn tokens
responding to every conversation.

### Chat ID Allowlists

Group chat detection is automatic.
Add the bot to a Telegram group and it works — group chats are always
allowed regardless of `allowed_chat_ids`, since they run as public agents.

Private chats still require `allowed_chat_ids` (or `allow_all_chats: true`):

```json
{
  "controllers": [
    {
      "type": "telegram",
      "bot_token": "...",
      "allowed_chat_ids": [
        123456789
      ]
    }
  ]
}
```

### Customizing the SSRF Policy

The sandbox policy for `web_fetch` can be overridden in `dyson.json`:

```json
{
  "sandbox": {
    "tool_policies": {
      "web_fetch": {
        "network": "deny"
      }
    }
  }
}
```

Setting `network: "deny"` disables `web_fetch` entirely. The SSRF check
(blocking internal IPs) is always applied when network access is allowed —
it cannot be disabled via configuration.

---

## Reload Behavior

When the config or workspace is reloaded (file change detected), all agents
are rebuilt. Public agents are rebuilt with `AgentMode::Public` — they stay
restricted and get a fresh read of the channel workspace (picking up any
identity changes that propagated through symlinks). The `is_group` flag is
stored per chat entry and preserved across reloads.

---

## Directory Layout

```
~/.dyson/                        # Main workspace (operator)
├── SOUL.md                      # Agent personality
├── IDENTITY.md                  # Agent identity
├── MEMORY.md                    # Operator memory
├── memory/                      # Operator journals
├── channels/                    # Per-channel workspaces
│   ├── -1001234567890/          # Telegram group chat
│   │   ├── SOUL.md → ../../SOUL.md       # Symlink (read-only)
│   │   ├── IDENTITY.md → ../../IDENTITY.md  # Symlink (read-only)
│   │   ├── MEMORY.md            # Channel-specific memory
│   │   ├── memory/              # Channel journals
│   │   └── memory.db            # Channel FTS5 index
│   └── -1009876543210/          # Another group chat
│       ├── SOUL.md → ../../SOUL.md
│       └── ...
└── memory.db                    # Operator FTS5 index
```
