# Configuration

Dyson loads settings from a JSON config file, environment variables, and CLI
flags.  All sources merge into a single `Settings` struct — the agent never
knows where a value came from.

**Key files:**
- `src/config/mod.rs` — `Settings`, `AgentSettings`, `LlmProvider`, `SkillConfig`
- `src/config/loader.rs` — JSON loader with env var / secret resolution

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
    pub dangerous_no_sandbox: bool,
}

pub struct AgentSettings {
    pub model: String,
    pub max_iterations: usize,
    pub max_tokens: u32,
    pub system_prompt: String,
    pub api_key: String,
    pub provider: LlmProvider,
    pub base_url: Option<String>,
}

pub enum LlmProvider {
    Anthropic,
    OpenAi,
    ClaudeCode,
    Codex,
}
```

| Field | Default | Env var | CLI flag |
|-------|---------|---------|----------|
| `model` | `claude-sonnet-4-20250514` | — | — |
| `max_iterations` | `20` | — | — |
| `max_tokens` | `8192` | — | — |
| `api_key` | (none) | `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` | — |
| `provider` | `Anthropic` | — | `--provider` |
| `base_url` | (provider default) | — | `--base-url` |
| `dangerous_no_sandbox` | `false` | — | `--dangerous-no-sandbox` |

---

## Config File: dyson.json

Dyson's native config format.  Example:

```json
{
  "agent": {
    "model": "claude-sonnet-4-20250514",
    "max_iterations": 50,
    "max_tokens": 16384,
    "system_prompt": "You are a helpful coding assistant.",
    "api_key": "sk-ant-...",
    "provider": "anthropic",
    "base_url": "https://api.anthropic.com"
  },
  "skills": {
    "builtin": {
      "tools": ["bash", "read_file"]
    },
    "mcp": [
      {
        "name": "github",
        "command": "npx",
        "args": ["-y", "@modelcontextprotocol/server-github"],
        "env": { "GITHUB_TOKEN": "$GITHUB_TOKEN" }
      },
      {
        "name": "linear",
        "url": "https://mcp.linear.app/sse"
      }
    ],
    "local": [
      {
        "name": "code",
        "path": "./skills/code/SKILL.md"
      }
    ]
  },
  "controllers": [
    { "type": "terminal" },
    { "type": "telegram", "bot_token": "$TELEGRAM_BOT_TOKEN" }
  ]
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
2. Environment variables  ANTHROPIC_API_KEY, OPENAI_API_KEY
3. Config file values     dyson.json "agent" section
4. Built-in defaults      model = "claude-sonnet-4-20250514", etc.
```

### API key resolution

The API key is required for Anthropic and OpenAI providers.  Claude Code and
Codex use their own stored credentials and don't need an API key.  Resolution
for API-based providers:

1. Check the provider-specific env var (`ANTHROPIC_API_KEY` or `OPENAI_API_KEY`)
2. Fall back to the `api_key` field in `dyson.json`
3. If neither is set → error with a clear message

---

## Env Var References in Config

MCP skill environment variables and secret references support `$ENVVAR` syntax:

```json
{
  "skills": {
    "mcp": [{
      "name": "github",
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "$GITHUB_TOKEN" }
    }]
  }
}
```

If the value starts with `$`, Dyson resolves it from the process environment
at load time.  This lets you reference secrets without hardcoding them in the
config file.

---

## Providers

Named provider configurations live in the `"providers"` map.  The `"agent"`
section references a provider by name.  This lets you define multiple providers
and switch between them at runtime (e.g. via `/model`).

```json
{
  "providers": {
    "claude": {
      "type": "anthropic",
      "model": "claude-sonnet-4-20250514",
      "api_key": { "resolver": "insecure_env", "name": "ANTHROPIC_API_KEY" }
    },
    "gpt": {
      "type": "openai",
      "model": "gpt-4o",
      "api_key": { "resolver": "insecure_env", "name": "OPENAI_API_KEY" },
      "base_url": "https://api.openai.com"
    },
    "local": {
      "type": "openai",
      "model": "llama-3.2-3b-instruct",
      "base_url": "http://localhost:9000"
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
| `type` | Yes | Provider backend: `"anthropic"`, `"openai"`, `"claude-code"`, `"codex"` |
| `model` | No | Model identifier (defaults per provider type) |
| `api_key` | No | API key — literal string or secret resolver reference |
| `base_url` | No | Override the default API endpoint |

### Provider Selection

The `provider` field determines which `LlmClient` implementation is used:

| Config value | Provider | Default endpoint | API key env var |
|-------------|----------|-----------------|----------------|
| `"anthropic"` or `"claude"` | Anthropic Messages API | `https://api.anthropic.com` | `ANTHROPIC_API_KEY` |
| `"openai"` or `"gpt"` | OpenAI Chat Completions | `https://api.openai.com` | `OPENAI_API_KEY` |
| `"claude-code"` or `"cc"` | Claude Code CLI | (subprocess) | None needed |
| `"codex"` or `"codex-cli"` | Codex CLI | (subprocess) | None needed |

Use `base_url` to point to alternative endpoints (Ollama, vLLM, Together,
etc.) when using the OpenAI provider.

---

## Skill Configuration

Three types of skills can be configured:

### Builtin

```json
{ "skills": { "builtin": { "tools": ["bash"] } } }
```

### MCP (stdio transport)

```json
{
  "skills": {
    "mcp": [{
      "name": "my-server",
      "command": "npx",
      "args": ["-y", "@example/mcp-server"],
      "env": { "API_KEY": "$MY_API_KEY" }
    }]
  }
}
```

### MCP (SSE transport)

```json
{
  "skills": {
    "mcp": [{
      "name": "remote-server",
      "url": "https://mcp.example.com/sse"
    }]
  }
}
```

### Local

Local skills can be configured explicitly in `dyson.json`:

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

---

## Future: Portable Config Loading

Dyson will support loading MCP server definitions from other tools' config
files:

| Source | File | Status |
|--------|------|--------|
| Dyson native | `dyson.json` | Implemented |
| Claude Desktop | `claude_desktop_config.json` | Planned |
| Cursor | `.cursor/mcp.json` | Planned |
| VS Code | `.vscode/mcp.json` | Planned |

All formats parse into the same `Settings` struct.  The agent never knows
which format was originally used.

---

See also: [Architecture Overview](architecture-overview.md) ·
[LLM Clients](llm-clients.md) · [Tools & Skills](tools-and-skills.md)
