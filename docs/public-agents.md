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

**Current support:** Telegram group chats (automatic detection). The
architecture is controller-agnostic — any future controller (HTTP API,
Discord, Slack) can reuse `build_group_agent()` and the SSRF sandbox for
its own public-facing mode.

**Key files:**
- `src/controller/telegram/mod.rs` — `build_group_agent()`, `get_or_create_entry()` (group detection)
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

## Telegram Integration

The Telegram controller detects group chats via the `type` field in the
Telegram Bot API's `Chat` object. When a message arrives from a group or
supergroup, the controller builds a public agent instead of the full-featured
one used for private chats.

```text
Message arrives from Telegram
  │
  ├── chat.type == Private     →  build_agent()        [full tools + workspace]
  └── chat.type == Group       →  build_group_agent()  [web_search + web_fetch only]
          or Supergroup
```

Each chat (private or group) gets its own agent instance and conversation
history, exactly like before. The only difference is which tools are loaded.

---

## Security Model

| Concern | Enforcement | Location |
|---------|-------------|----------|
| Tool restriction | `BuiltinSkill::new_filtered()` with allowlist `["web_search", "web_fetch"]` | `build_group_agent()` |
| No bash/file access | Tools not in registry — LLM cannot call them | `skill/builtin.rs` |
| No workspace tools | No workspace passed to `Agent::builder()` — workspace tools have nothing to operate on | `build_group_agent()` |
| No dreams | `nudge_interval = 0`, no workspace — dream system never fires | `build_group_agent()` |
| SSRF protection | `PolicySandbox` blocks internal/private IPs for `web_fetch` | `policy_sandbox.rs` |
| Sandbox always active | `create_sandbox(config, false)` — hardcoded, ignores `--dangerous-no-sandbox` | `build_group_agent()` |
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

Any controller can build a public agent by calling `build_group_agent()`
(or reimplementing the same pattern):

1. **Create restricted skills** — `BuiltinSkill::new_filtered()` with only
   `["web_search", "web_fetch"]`.
2. **Create client and sandbox** — pass `false` for `dangerous_no_sandbox`
   to both `create_client()` and `create_sandbox()`. This is the critical
   security invariant: the sandbox must always be active for public agents,
   even if the operator started Dyson with `--dangerous-no-sandbox`.
3. **Build via `Agent::builder()`** — no `.workspace()`, no `.nudge_interval()`.
4. **Detect the public context** — Telegram uses chat type; an HTTP
   controller might use a config flag or endpoint path.

---

## Configuration

No special configuration is needed. Group chat detection is automatic.
Add the bot to a Telegram group and it works.

The group must be in the `allowed_chat_ids` list (or `allow_all_chats: true`):

```json
{
  "controllers": [
    {
      "type": "telegram",
      "bot_token": "...",
      "allowed_chat_ids": [
        -1001234567890
      ]
    }
  ]
}
```

Group chat IDs are negative numbers in Telegram. Use the `/whoami` command
in the group to discover the chat ID.

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
are rebuilt. Public agents are rebuilt with `build_group_agent()` — they
stay restricted. The `is_group` flag is stored per chat entry and preserved
across reloads.
