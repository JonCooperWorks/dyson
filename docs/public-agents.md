# Public Agents (Group Chat Mode)

When Dyson's Telegram bot is added to a group chat, it automatically runs in
**restricted mode** — a hardened configuration with only `web_search` and
`web_fetch` tools. This prevents group members from using the bot to access
the filesystem, run shell commands, or modify the agent's workspace.

**Key files:**
- `src/controller/telegram/mod.rs` — `build_group_agent()`, `get_or_create_entry()` (group detection)
- `src/controller/telegram/types.rs` — `ChatType` enum, `Chat::is_group()`
- `src/sandbox/policy_sandbox.rs` — `check_web_fetch()`, `check_url_not_internal()` (SSRF protection)
- `src/sandbox/policy.rs` — `web_fetch` default policy entry

---

## How It Works

The Telegram controller detects group chats via the `type` field in the
Telegram Bot API's `Chat` object. When a message arrives from a group or
supergroup, the controller builds a restricted agent instead of the
full-featured one used for private chats.

```text
Message arrives from Telegram
  │
  ├── chat.type == "private"  →  build_agent()       [full tools + workspace]
  └── chat.type == "group"    →  build_group_agent()  [web_search + web_fetch only]
          or "supergroup"
```

Each chat (private or group) gets its own agent instance and conversation
history, exactly like before. The only difference is which tools are loaded.

---

## Security Model

| Concern | Enforcement | Location |
|---------|-------------|----------|
| Tool restriction | `BuiltinSkill::new_filtered()` with allowlist `["web_search", "web_fetch"]` | `build_group_agent()` |
| No bash/file access | Tools not in registry — LLM cannot call them | `skill/builtin.rs` |
| No workspace tools | No workspace passed to `Agent::new()` — workspace tools have nothing to operate on | `build_group_agent()` |
| No dreams | `nudge_interval = 0`, no workspace — dream system never fires | `build_group_agent()` |
| SSRF protection | `PolicySandbox` blocks internal/private IPs for `web_fetch` | `policy_sandbox.rs` |
| Sandbox always active | `create_sandbox(config, false)` — hardcoded, ignores `--dangerous-no-sandbox` | `build_group_agent()` |
| Per-chat isolation | Same `HashMap<i64, Arc<ChatEntry>>` as private chats | `get_or_create_entry()` |

### SSRF Protection

The `web_fetch` tool lets the LLM fetch arbitrary URLs. In a group chat
(or any public-facing context), this creates an SSRF risk — the LLM could
be tricked into fetching internal services.

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

The SSRF check is implemented in the sandbox, not in the `web_fetch` tool
itself. This means it's enforced regardless of how `web_fetch` is called
and cannot be bypassed by prompt injection.

---

## Configuration

No special configuration is needed. Group chat detection is automatic.
Just add the bot to a group and it works.

The bot must be in the `allowed_chat_ids` list (or `allow_all_chats: true`):

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

### Customizing the SSRF policy

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
are rebuilt. Group chat agents are rebuilt with `build_group_agent()` — they
stay restricted. The `is_group` flag is stored per chat entry and preserved
across reloads.
