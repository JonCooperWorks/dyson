# Configuration

Dyson loads settings from a JSON config file, environment variables, and CLI
flags.  All sources merge into a single `Settings` struct — the agent never
knows where a value came from.

**Key files:**
- `src/config/mod.rs` — `Settings`, `AgentSettings`, `LlmProvider`, `SkillConfig`
- `src/config/loader.rs` — JSON loader with env var / secret resolution
- `src/config/migrate.rs` — Declarative migration chain for config upgrades

---

## Settings Struct

```rust
pub struct Settings {
    pub agent: AgentSettings,
    pub providers: HashMap<String, ProviderConfig>,
    pub skills: Vec<SkillConfig>,
    pub controllers: Vec<ControllerConfig>,
    pub sandbox: SandboxConfig,
    pub workspace: WorkspaceConfig,
    pub chat_history: ChatHistoryConfig,
    pub web_search: Option<WebSearchConfig>,
    pub dangerous_no_sandbox: bool,
}

pub struct AgentSettings {
    pub model: String,
    pub max_iterations: usize,
    pub max_tokens: u32,
    pub system_prompt: String,
    pub api_key: Credential,
    pub provider: LlmProvider,
    pub base_url: Option<String>,
    pub compaction: Option<CompactionConfig>,
    pub rate_limit: Option<RateLimitConfig>,
}

pub enum LlmProvider {
    Anthropic,
    OpenAi,
    OpenRouter,
    ClaudeCode,
    Codex,
    OllamaCloud,
}
```

| Field | Default | Env var | CLI flag |
|-------|---------|---------|----------|
| `model` | `claude-sonnet-4-20250514` | — | — |
| `max_iterations` | `20` | — | — |
| `max_tokens` | `8192` | — | — |
| `api_key` | (none) | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, or `OLLAMA_API_KEY` | — |
| `provider` | `Anthropic` | — | `--provider` |
| `base_url` | (provider default) | — | `--base-url` |
| `dangerous_no_sandbox` | `false` | — | `--dangerous-no-sandbox` |

---

## Config Versioning

Dyson uses a `config_version` field to track the config format.  The current
version is **2**.  Configs without a `config_version` field are treated as
version 0 and migrated automatically.

| Migration | What it does |
|-----------|-------------|
| v0 → v1 | Moves inline `agent.provider`/`api_key`/`base_url` into a `"providers"` map |
| v1 → v2 | Renames `providers.*.model` (string) to `providers.*.models` (array) |

When migration is applied, the loader writes the updated config back to disk
so subsequent loads skip already-applied migrations.

---

## Config File: dyson.json

Dyson's native config format.  Example:

```json
{
  "config_version": 2,
  "providers": {
    "default": {
      "type": "claude-code",
      "models": ["sonnet"]
    }
  },
  "agent": {
    "provider": "default",
    "max_iterations": 20,
    "max_tokens": 8192,
    "system_prompt": "You are a helpful coding assistant."
  },
  "mcp_servers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "$GITHUB_TOKEN" }
    },
    "context7": {
      "url": "https://mcp.context7.com/mcp",
      "headers": {
        "CONTEXT7_API_KEY": { "resolver": "insecure_env", "name": "CONTEXT7_API_KEY" }
      }
    }
  },
  "skills": {
    "builtin": {
      "tools": ["bash", "read_file"]
    },
    "local": [
      {
        "name": "code",
        "path": "./skills/code/SKILL.md"
      }
    ],
    "subagents": [
      {
        "name": "research_agent",
        "description": "Research specialist for web research tasks.",
        "system_prompt": "You are a research specialist.",
        "provider": "default",
        "max_iterations": 15
      }
    ]
  },
  "controllers": [
    { "type": "terminal" },
    {
      "type": "telegram",
      "bot_token": { "resolver": "insecure_env", "name": "TELEGRAM_API_KEY" },
      "allowed_chat_ids": [
        { "resolver": "insecure_env", "name": "TELEGRAM_ALLOWED_CHAT_ID" }
      ]
    }
  ],
  "workspace": {
    "backend": "openclaw",
    "connection_string": "~/.dyson",
    "memory": {
      "limits": { "MEMORY.md": 2500, "USER.md": 1375 },
      "overflow_factor": 1.35,
      "nudge_interval": 7
    }
  }
}
```

---

## File Discovery

When no `--config` flag is provided, Dyson searches:

1. `./dyson.json` in the current working directory
2. `~/.config/dyson/dyson.json` (global config)
3. No file found → use built-in defaults

With `--config path/to/custom.json`, only that file is loaded (error if
missing).

---

## Resolution Order

Higher priority wins:

```
1. CLI flags              --provider openai --base-url http://...
2. Config file values     dyson.json
3. Environment variables  ANTHROPIC_API_KEY, OPENAI_API_KEY, etc.
4. Built-in defaults      model = "claude-sonnet-4-20250514", etc.
```

### API key resolution

The API key is required for Anthropic, OpenAI, OpenRouter, and Ollama Cloud
providers.  Claude Code and Codex use their own stored credentials and don't
need an API key.  Resolution for API-based providers:

1. Check the `api_key` field in the provider's config entry
2. Fall back to the provider-specific env var (`ANTHROPIC_API_KEY`,
   `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, or `OLLAMA_API_KEY`)
3. If neither is set → error with a clear message

**Security:** When a custom `base_url` is set, env-var fallback is blocked —
the API key must be provided explicitly.  This prevents accidentally sending
your Anthropic key to a third-party endpoint.

---

## Env Var References in Config

MCP skill environment variables, controller configs, and secret references
support `$ENVVAR` syntax:

```json
{
  "mcp_servers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "$GITHUB_TOKEN" }
    }
  }
}
```

If the value starts with `$`, Dyson resolves it from the process environment
at load time.  This lets you reference secrets without hardcoding them in the
config file.

For structured secret resolution, use the resolver syntax:

```json
{
  "bot_token": { "resolver": "insecure_env", "name": "TELEGRAM_API_KEY" }
}
```

See [Secrets](secrets.md) for the full scheme system.

---

## Providers

Named provider configurations live in the `"providers"` map.  The `"agent"`
section references a provider by name.  This lets you define multiple providers
and switch between them at runtime (e.g. via `/model`).

```json
{
  "config_version": 2,
  "providers": {
    "claude": {
      "type": "anthropic",
      "models": ["claude-sonnet-4-20250514", "claude-opus-4-20250514"],
      "api_key": { "resolver": "insecure_env", "name": "ANTHROPIC_API_KEY" }
    },
    "gpt": {
      "type": "openai",
      "models": ["gpt-4o"],
      "api_key": { "resolver": "insecure_env", "name": "OPENAI_API_KEY" },
      "base_url": "https://api.openai.com"
    },
    "local": {
      "type": "openai",
      "models": ["llama-3.2-3b-instruct"],
      "base_url": "http://localhost:9000"
    },
    "router": {
      "type": "openrouter",
      "models": ["anthropic/claude-sonnet-4"],
      "api_key": { "resolver": "insecure_env", "name": "OPENROUTER_API_KEY" }
    }
  },
  "agent": {
    "provider": "claude"
  }
}
```

Each provider entry has:

| Field | Required | Description |
|-------|----------|-------------|
| `type` | Yes | Provider backend (see table below) |
| `models` | No | Array of model identifiers.  First entry is the default.  Defaults per provider type. |
| `api_key` | No | API key — literal string or secret resolver reference |
| `base_url` | No | Override the default API endpoint |

### Provider Selection

The `type` field determines which `LlmClient` implementation is used:

| Config value | Aliases | Provider | Default model | API key env var |
|-------------|---------|----------|---------------|----------------|
| `"anthropic"` | — | Anthropic Messages API | `claude-sonnet-4-20250514` | `ANTHROPIC_API_KEY` |
| `"openai"` | `"gpt"` | OpenAI Chat Completions | `gpt-4o` | `OPENAI_API_KEY` |
| `"openrouter"` | `"open-router"`, `"open_router"` | OpenRouter | `anthropic/claude-sonnet-4` | `OPENROUTER_API_KEY` |
| `"claude-code"` | `"claude_code"`, `"cc"` | Claude Code CLI | `claude-sonnet-4-20250514` | None needed |
| `"codex"` | `"codex-cli"` | Codex CLI | `codex` | None needed |
| `"ollama-cloud"` | `"ollama_cloud"`, `"ollama"` | Ollama Cloud | `llama3.3` | `OLLAMA_API_KEY` |

Use `base_url` to point to alternative endpoints (vLLM, Together,
etc.) when using the OpenAI provider.

---

## MCP Servers

MCP servers are configured in the top-level `"mcp_servers"` object.  Each
key is the server name, and the value defines the transport.

### Stdio transport

Spawn a local process and communicate over stdin/stdout:

```json
{
  "mcp_servers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "$GITHUB_TOKEN" }
    }
  }
}
```

### HTTP transport

POST JSON-RPC to an HTTP endpoint (Streamable HTTP MCP):

```json
{
  "mcp_servers": {
    "context7": {
      "url": "https://mcp.context7.com/mcp",
      "headers": { "API_KEY": "..." }
    }
  }
}
```

### HTTP with OAuth

For MCP servers that require OAuth 2.0 authorization:

```json
{
  "mcp_servers": {
    "github-copilot": {
      "url": "https://mcp.example.com/mcp",
      "auth": {
        "type": "oauth",
        "scopes": ["read", "write"]
      }
    }
  }
}
```

Minimal config uses auto-discovery and Dynamic Client Registration.  For
pre-registered clients:

```json
{
  "mcp_servers": {
    "example": {
      "url": "https://mcp.example.com/mcp",
      "auth": {
        "type": "oauth",
        "client_id": "my-client-id",
        "client_secret": { "resolver": "insecure_env", "name": "CLIENT_SECRET" },
        "scopes": ["read", "write"],
        "authorization_url": "https://auth.example.com/authorize",
        "token_url": "https://auth.example.com/token"
      }
    }
  }
}
```

See [MCP OAuth](mcp-oauth.md) for the full flow.

---

## Skill Configuration

Four types of skills can be configured in the `"skills"` section:

### Builtin

```json
{ "skills": { "builtin": { "tools": ["bash", "read_file"] } } }
```

Omitting the `"builtin"` section includes all built-in tools.  An empty
array `"tools": []` explicitly disables all builtins.

### Local

Local skills can be configured explicitly:

```json
{
  "skills": {
    "local": [{
      "name": "my-skill",
      "path": "./skills/my-skill/SKILL.md"
    }]
  }
}
```

They can also be auto-discovered from the workspace's `skills/` directory
(Hermes-style).  Any `.md` file in `~/.dyson/skills/` with valid SKILL.md
frontmatter is loaded automatically — no config needed.  See
[Tools & Skills](tools-and-skills.md#localskill--workspace-managed-skills)
for the SKILL.md format.

### Subagents

Subagents are child agents exposed as tools to the parent:

```json
{
  "skills": {
    "subagents": [
      {
        "name": "research_agent",
        "description": "Research specialist for web research tasks.",
        "system_prompt": "You are a research specialist.",
        "provider": "gpt",
        "max_iterations": 15,
        "max_tokens": 4096,
        "tools": ["web_search", "read_file"]
      }
    ]
  }
}
```

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `name` | Yes | — | Tool name exposed to the parent LLM |
| `description` | Yes | — | Human-readable description for the parent LLM |
| `system_prompt` | Yes | — | System prompt for the subagent |
| `provider` | Yes | — | Provider name from the `"providers"` map |
| `model` | No | Provider default | Model override |
| `max_iterations` | No | `10` | Maximum LLM turns per invocation |
| `max_tokens` | No | `4096` | Maximum tokens per response |
| `tools` | No | All parent tools | Tool name filter (subset of parent tools) |

Subagents share the parent's sandbox and workspace but run isolated
conversations.  Max nesting depth is 3.  See [Subagents](subagents.md).

---

## Compaction

Context compaction automatically summarizes conversation history when the
estimated context size exceeds a threshold.  Configure under `agent.compaction`:

```json
{
  "agent": {
    "compaction": {
      "context_window": 200000,
      "threshold_ratio": 0.50,
      "protect_head": 3,
      "protect_tail_tokens": 20000,
      "summary_min_tokens": 2000,
      "summary_max_tokens": 12000,
      "summary_target_ratio": 0.20
    }
  }
}
```

Shorthand — just the context window size (all other fields use defaults):

```json
{ "agent": { "compaction": 200000 } }
```

| Field | Default | Description |
|-------|---------|-------------|
| `context_window` | `200000` | Model's context window in estimated tokens |
| `threshold_ratio` | `0.50` | Fraction of context_window that triggers compaction |
| `protect_head` | `3` | Messages always kept at the start |
| `protect_tail_tokens` | `20000` | Token budget for recent messages kept verbatim |
| `summary_min_tokens` | `2000` | Minimum tokens for the summary |
| `summary_max_tokens` | `12000` | Maximum tokens for the summary |
| `summary_target_ratio` | `0.20` | Target ratio of summary size to middle section |

When absent, compaction is disabled.  See [Memory](memory.md) for the full
algorithm.

---

## Rate Limiting

Per-agent rate limiting controls how many messages are processed within a
sliding time window:

```json
{
  "agent": {
    "rate_limit": {
      "max_messages": 30,
      "window_secs": 60
    }
  }
}
```

When absent, no rate limit is applied.

---

## Sandbox

```json
{
  "sandbox": {
    "disabled": ["os"],
    "os_profile": "strict",
    "tool_policies": {
      "web_search": { "network": "allow", "file_read": "deny" },
      "mcp__*": { "network": "allow", "file_write": "deny" }
    }
  }
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `disabled` | `[]` | Sandbox names to disable (e.g., `"os"`) |
| `os_profile` | `"default"` | Fallback profile when `tool_policies` is empty |
| `tool_policies` | `{}` | Per-tool or glob policies (overrides defaults) |

See [Sandbox](sandbox.md) for the full capability model and policy system.

---

## Workspace

```json
{
  "workspace": {
    "backend": "openclaw",
    "connection_string": "~/.dyson",
    "memory": {
      "limits": { "MEMORY.md": 2500, "USER.md": 1375 },
      "overflow_factor": 1.35,
      "nudge_interval": 7
    }
  }
}
```

Legacy shorthand (still supported):

```json
{ "workspace": { "path": "~/.dyson" } }
```

| Field | Default | Description |
|-------|---------|-------------|
| `backend` | `"openclaw"` | Workspace backend type |
| `connection_string` | `"~/.dyson"` | Path (supports secret resolver) |
| `memory.limits` | `{ "MEMORY.md": 2500, "USER.md": 1375 }` | Per-file **soft** character targets (curator aims here) |
| `memory.overflow_factor` | `1.35` | Multiplier — hard ceiling = soft target × factor. Writes between the two succeed with a warning. |
| `memory.nudge_interval` | `7` | Inject memory maintenance nudge every N turns (0 = disabled) |

---

## Chat History

```json
{
  "chat_history": {
    "backend": "disk",
    "connection_string": "~/.dyson/chats"
  }
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `backend` | `"disk"` | Backend type |
| `connection_string` | `"~/.dyson/chats"` | Directory path (supports secret resolver) |

---

## Web Search

The `web_search` section enables the `web_search` built-in tool.  When
absent, the tool doesn't exist — models never see it.

### Brave Search (requires API key)

```json
{
  "web_search": {
    "provider": "brave",
    "api_key": { "resolver": "insecure_env", "name": "BRAVE_API_KEY" }
  }
}
```

Free tier: 2000 queries/month.  Get a key at https://brave.com/search/api/.

### SearXNG (no API key needed)

```json
{
  "web_search": {
    "provider": "searxng",
    "base_url": "https://searx.be"
  }
}
```

Use any public instance from https://searx.space/ or a self-hosted one.

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `provider` | No | `"brave"` | Search backend: `"brave"` or `"searxng"` |
| `api_key` | For Brave | — | API key (literal or secret resolver reference) |
| `base_url` | For SearXNG | — | Instance URL (e.g. `"https://searx.be"`) |

---

## Web Browsing

Dyson offers two ways to fetch and read web page content.

### Built-in `web_fetch` tool

The `web_fetch` tool is always available — no configuration needed.  It
fetches a URL and returns clean extracted text, stripping HTML tags, scripts,
and styles.  This saves tokens compared to `curl` via bash, which returns raw
HTML.

Supported content types:
- **text/html** — stripped to plain text via `nanohtml2text`
- **text/plain** — returned as-is
- **application/json** — pretty-printed

Limits: 30 s timeout, 5 MB max response body, configurable output length
(default 50 000 chars, max 200 000).

### MCP browsing servers

For full browser automation (JavaScript rendering, clicking, screenshots),
add a browsing MCP server.  No code changes needed — Dyson discovers the
server's tools automatically.

**Fetch-only (lightweight, no browser):**

```json
{
  "mcp_servers": {
    "fetch": {
      "command": "npx",
      "args": ["-y", "@anthropic/mcp-server-fetch"]
    }
  }
}
```

**Full browser automation (Playwright):**

```json
{
  "mcp_servers": {
    "browser": {
      "command": "npx",
      "args": ["-y", "@anthropic/mcp-server-playwright"]
    }
  }
}
```

The built-in `web_fetch` tool covers the common case (read a page, get the
text).  Use an MCP browsing server when you need JavaScript rendering,
form interaction, or screenshots.

---

## Controllers

Controllers define how Dyson interacts with the outside world.  Each entry
has a `type` field and controller-specific fields in the same object:

```json
{
  "controllers": [
    { "type": "terminal" },
    {
      "type": "telegram",
      "bot_token": { "resolver": "insecure_env", "name": "TELEGRAM_API_KEY" },
      "allowed_chat_ids": [
        { "resolver": "insecure_env", "name": "TELEGRAM_ALLOWED_CHAT_ID" }
      ]
    }
  ]
}
```

Controller config is opaque — each implementation deserializes its own fields
from the JSON blob.  Values support the `$ENVVAR` syntax and secret resolver
references.  If no controllers are configured, defaults to a single terminal
controller.

---

See also: [Architecture Overview](architecture-overview.md) ·
[LLM Clients](llm-clients.md) · [Tools & Skills](tools-and-skills.md) ·
[Secrets](secrets.md) · [Subagents](subagents.md) · [MCP OAuth](mcp-oauth.md)
