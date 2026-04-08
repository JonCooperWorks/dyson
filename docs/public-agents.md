# Public Agents

A **public agent** is a Dyson agent exposed to untrusted users — people who
didn't configure it and shouldn't have access to the host machine. A private
agent (the default) has full tools: bash, file I/O, workspace, skills. A
public agent is stripped down to safe, read-only-web tools and hardened
against abuse.

The core problem: an LLM agent with shell access is powerful for its owner
but dangerous when strangers can talk to it. Prompt injection in a private
chat is the owner's risk to manage. Prompt injection in a group chat or
public API means anyone can try to make the agent `rm -rf /`, read
`/etc/shadow`, or hit the cloud metadata endpoint. Public agents solve this
by removing the dangerous tools entirely and enforcing SSRF protection on
what remains.

**Current support:** Telegram group chats (automatic — just add the bot to
a group). The public/private decision is made at the controller level via a
single `build_agent(settings, prompt, public)` call. Any future controller
(HTTP API, Discord, Slack) uses the same function with `public: true`.

**Key files:**
- `src/controller/mod.rs` — `build_agent()` with `public` flag, `build_public_agent()` (the single configuration point)
- `src/controller/telegram/mod.rs` — `get_or_create_entry()` passes `is_group` as the `public` flag
- `src/controller/telegram/types.rs` — `ChatType` enum, `Chat::is_group()`
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
| Workspace (memory, identity) | Yes | **No** |
| Load/create skills | Yes | **No** |
| MCP tools | Yes | **No** |
| Subagents | Yes | **No** |
| Dreams (background cognition) | Yes | **No** |

A public agent is a web research assistant. It can answer questions using
search and page fetching, but it cannot touch the host system.

---

## How It Works

The `build_agent()` function in `src/controller/mod.rs` is the single point
of control. It takes a `public: bool` parameter:

```rust
pub async fn build_agent(
    settings: &Settings,
    controller_prompt: Option<&str>,
    public: bool,                        // ← controllers set this
) -> Result<Agent>
```

Controllers don't configure tools, sandbox, or workspace themselves — they
just declare whether a session is public, and `build_agent` handles the rest.

### Telegram Integration

The Telegram controller detects group chats via the `type` field in the
Telegram Bot API's `Chat` object. Group chats are automatically allowed
without being in `allowed_chat_ids` — they always run as public agents,
so they're safe without explicit whitelisting.

```text
Message arrives from Telegram
  │
  ├── chat.type == Private     →  build_agent(settings, prompt, false)  [full agent]
  └── chat.type == Group       →  build_agent(settings, prompt, true)   [public agent]
          or Supergroup
```

Each chat gets its own agent instance and conversation history. The only
difference is which tools are loaded.

---

## Security Model

| Concern | Enforcement | Location |
|---------|-------------|----------|
| Tool restriction | `BuiltinSkill::new_filtered()` with allowlist `["web_search", "web_fetch"]` | `build_group_agent()` |
| No bash/file access | Tools not in registry — LLM cannot call them | `skill/builtin.rs` |
| No workspace tools | No workspace passed to `Agent::builder()` — workspace tools have nothing to operate on | `build_public_agent()` |
| No dreams | `nudge_interval = 0`, no workspace — dream system never fires | `build_public_agent()` |
| SSRF protection | `PolicySandbox` blocks internal/private IPs for `web_fetch` | `policy_sandbox.rs` |
| Sandbox always active | `create_sandbox(config, false)` — hardcoded, ignores `--dangerous-no-sandbox` | `build_public_agent()` |
| Groups auto-allowed | Group chats bypass `allowed_chat_ids` — they're always public and safe | Telegram controller |
| Per-chat isolation | Same `HashMap<i64, Arc<ChatEntry>>` as private chats | `get_or_create_entry()` |

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

---

## Adding Public Agent Support to a New Controller

Call `build_agent(settings, prompt, true)`. That's it. The controller module
handles tool filtering, sandbox enforcement, and workspace omission. The
controller just needs to decide *when* to pass `true`:

- **Telegram**: `msg.chat.is_group()` — group chats are public.
- **HTTP API**: could be a config flag, endpoint path, or auth level.
- **Discord**: public channels vs DMs.

---

## Configuration

No special configuration is needed. Group chat detection is automatic.
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
are rebuilt. Public agents are rebuilt with `build_agent(settings, prompt, true)`
— they stay restricted. The `is_group` flag is stored per chat entry and
preserved across reloads.
