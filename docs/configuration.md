# Configuration

Dyson loads settings from a TOML config file, environment variables, and CLI
flags.  All sources merge into a single `Settings` struct — the agent never
knows where a value came from.

**Key files:**
- `src/config/mod.rs` — `Settings`, `AgentSettings`, `LlmProvider`, `SkillConfig`
- `src/config/dyson_toml.rs` — TOML loader with env var resolution

---

## Settings Struct

```rust
pub struct Settings {
    pub agent: AgentSettings,
    pub skills: Vec<SkillConfig>,
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

---

## Config File: dyson.toml

Dyson's native config format.  Example:

```toml
[agent]
model = "claude-sonnet-4-20250514"
max_iterations = 50
max_tokens = 16384
system_prompt = "You are a helpful coding assistant."
api_key = "sk-ant-..."             # prefer env var instead
provider = "anthropic"              # "anthropic" or "openai"
base_url = "https://api.anthropic.com"  # optional override

# Built-in tools
[skills.builtin]
tools = ["bash", "read_file"]       # empty = all builtins

# MCP servers
[[skills.mcp]]
name = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "$GITHUB_TOKEN" }

[[skills.mcp]]
name = "linear"
url = "https://mcp.linear.app/sse"

# Local skills
[[skills.local]]
name = "code"
path = "./skills/code/SKILL.md"
```

---

## File Discovery

When no `--config` flag is provided, Dyson searches:

1. `./dyson.toml` in the current working directory
2. `~/.config/dyson/dyson.toml` (global config)
3. No file found → use built-in defaults

With `--config path/to/custom.toml`, only that file is loaded (error if
missing).

---

## Resolution Order

Higher priority wins:

```
1. CLI flags              --provider openai --base-url http://...
2. Environment variables  ANTHROPIC_API_KEY, OPENAI_API_KEY
3. Config file values     dyson.toml [agent] section
4. Built-in defaults      model = "claude-sonnet-4-20250514", etc.
```

### API key resolution

The API key is critical — without it, nothing works.  Resolution:

1. Check the provider-specific env var (`ANTHROPIC_API_KEY` or `OPENAI_API_KEY`)
2. Fall back to the `api_key` field in `dyson.toml`
3. If neither is set → error with a clear message

---

## Env Var References in Config

MCP skill environment variables support `$ENVVAR` syntax:

```toml
[[skills.mcp]]
name = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "$GITHUB_TOKEN" }
```

If the value starts with `$`, Dyson resolves it from the process environment
at load time.  This lets you reference secrets without hardcoding them in the
config file.

---

## Provider Selection

The `provider` field determines which `LlmClient` implementation is used:

| Config value | Provider | Default endpoint | API key env var |
|-------------|----------|-----------------|----------------|
| `"anthropic"` or `"claude"` | Anthropic Messages API | `https://api.anthropic.com` | `ANTHROPIC_API_KEY` |
| `"openai"` or `"gpt"` or `"codex"` or `"claude-code"` | OpenAI Chat Completions | `https://api.openai.com` | `OPENAI_API_KEY` |

Use `base_url` to point to alternative endpoints (Ollama, vLLM, Together,
Codex CLI local server, etc.).

---

## Skill Configuration

Three types of skills can be configured:

### Builtin

```toml
[skills.builtin]
tools = ["bash"]            # list specific tools, or [] for all
```

### MCP (stdio transport)

```toml
[[skills.mcp]]
name = "my-server"
command = "npx"
args = ["-y", "@example/mcp-server"]
env = { API_KEY = "$MY_API_KEY" }
```

### MCP (SSE transport)

```toml
[[skills.mcp]]
name = "remote-server"
url = "https://mcp.example.com/sse"
```

### Local

```toml
[[skills.local]]
name = "my-skill"
path = "./skills/my-skill/SKILL.md"
```

Note: MCP and Local skills are defined in the config but not yet implemented
(Phase 1 only includes Builtin).  The config format is forward-compatible.

---

## Future: Portable Config Loading

Dyson will support loading MCP server definitions from other tools' config
files:

| Source | File | Status |
|--------|------|--------|
| Dyson native | `dyson.toml` | Implemented |
| Claude Desktop | `claude_desktop_config.json` | Planned |
| Cursor | `.cursor/mcp.json` | Planned |
| VS Code | `.vscode/mcp.json` | Planned |

All formats parse into the same `Settings` struct.  The agent never knows
which format was originally used.

---

See also: [Architecture Overview](architecture-overview.md) ·
[LLM Clients](llm-clients.md) · [Tools & Skills](tools-and-skills.md)
