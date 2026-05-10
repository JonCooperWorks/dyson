# Configuration

Dyson loads a JSON config file, applies schema migrations, resolves secrets,
then merges CLI overrides into one `Settings` value. Runtime code does not
care whether a value came from `dyson.json`, an environment-backed resolver, or
a command-line flag.

Key files:

- `crates/dyson/src/config/mod.rs`
- `crates/dyson/src/config/loader.rs`
- `crates/dyson/src/config/migrate.rs`
- `crates/dyson/src/command/`

## CLI Shape

```text
dyson listen [--config PATH] [--provider NAME] [--base-url URL] [--workspace DIR]
dyson init [--noinput] [--daemonize] [--path DIR]
dyson hash-bearer <plaintext>
dyson swarm
dyson run [OPTIONS] <prompt>
```

`dyson swarm` is not a normal user config path. It is started by
`dyson-swarm`, reads `SWARM_*` environment variables, renders a generated
config, and starts the HTTP controller inside the sandbox.

## File Discovery

Without `--config`, Dyson searches:

1. `./dyson.json`
2. `~/.config/dyson/dyson.json`
3. built-in defaults

The built-in defaults do not select a billable model. A usable interactive or
one-shot config must resolve an active provider with at least one model.

## Schema Version

The current `config_version` is **3**.

| Migration | Behaviour |
|---|---|
| v0 -> v1 | Moves inline `agent.provider`, `agent.api_key`, and `agent.base_url` into `providers.default` |
| v1 -> v2 | Renames each provider's `model` string to a `models` array and removes `agent.model` |
| v2 -> v3 | Marker migration for OIDC controller `allowed_sub`; no structural rewrite |

The loader writes migrated configs back to disk so later starts skip the same
work. A config with a version newer than the binary refuses to load.

## Minimal Config

```json
{
  "config_version": 3,
  "providers": {
    "claude": {
      "type": "anthropic",
      "models": ["claude-sonnet-4-20250514"],
      "api_key": { "resolver": "insecure_env", "name": "ANTHROPIC_API_KEY" }
    }
  },
  "agent": {
    "provider": "claude",
    "max_iterations": 40,
    "max_tokens": 8192
  },
  "controllers": [
    { "type": "terminal" }
  ]
}
```

## Provider Map

Named providers live in the top-level `providers` map. `agent.provider`
selects one by name.

Each provider entry uses:

| Field | Required | Notes |
|---|---|---|
| `type` | yes | Provider backend or alias |
| `models` | yes | Non-empty array; first model is the active default |
| `api_key` | API providers | Literal string or secret resolver |
| `base_url` | no | Alternate API endpoint |

Provider types:

| Config value | Aliases | Credential fallback | Notes |
|---|---|---|---|
| `anthropic` | | `ANTHROPIC_API_KEY` | Anthropic Messages API |
| `openai` | `gpt` | `OPENAI_API_KEY` | OpenAI Chat Completions; compatible APIs through `base_url` |
| `openrouter` | `open-router`, `open_router` | `OPENROUTER_API_KEY` | OpenAI-compatible OpenRouter wrapper |
| `gemini` | `google` | `GEMINI_API_KEY` | Gemini streaming and image generation |
| `ollama-cloud` | `ollama_cloud`, `ollama` | `OLLAMA_API_KEY` | Ollama Cloud |
| `claude-code` | `claude_code`, `cc` | none | Uses the local `claude` CLI credentials |
| `codex` | `codex-cli` | none | Uses the local `codex` CLI credentials |

If an API-key provider has `base_url`, env-var fallback is disabled. Put the
intended key in that provider entry so a default provider key is not silently
sent to a third-party endpoint.

Example with multiple providers:

```json
{
  "config_version": 3,
  "providers": {
    "claude": {
      "type": "anthropic",
      "models": ["claude-sonnet-4-20250514"],
      "api_key": { "resolver": "insecure_env", "name": "ANTHROPIC_API_KEY" }
    },
    "local": {
      "type": "openai",
      "models": ["qwen2.5-coder"],
      "base_url": "http://127.0.0.1:8000/v1",
      "api_key": "local-dev-key"
    },
    "codex": {
      "type": "codex",
      "models": ["codex"]
    }
  },
  "agent": { "provider": "claude" }
}
```

## Agent Fields

Common `agent` keys:

| Field | Default | Notes |
|---|---|---|
| `provider` | required for useful config | Name in `providers` |
| `max_iterations` | `40` | Maximum LLM turns per `run()` call |
| `max_retries` | `6` | Transient LLM retry budget |
| `max_concurrent_llm_calls` | `4` | Shared provider concurrency cap; `0` disables |
| `max_tokens` | `8192` | Per-turn output token limit |
| `system_prompt` | built-in grounded assistant prompt | Base prompt before skill fragments |
| `smartest_model` | unset | Advisor model in `provider/model` form |
| `image_generation_provider` | unset | Name of an image-capable provider |
| `image_generation_model` | unset | Optional image model override |
| `rate_limit` | unset | Per-agent message rate limit |
| `compaction` | built-in defaults | Context compaction thresholds |

## Controllers

Controllers are declared in the top-level `controllers` array.

```json
{
  "controllers": [
    { "type": "terminal" },
    { "type": "http", "bind": "127.0.0.1:7878" },
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

HTTP controller auth supports `dangerous_no_auth`, hashed `bearer`, and `oidc`.
On loopback, omitted auth defaults to `dangerous_no_auth`. On any non-loopback
bind, auth is required. See [Web UI / HTTP Controller](web.md).

## MCP Servers

MCP servers are configured in `mcp_servers`.

Stdio:

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

HTTP:

```json
{
  "mcp_servers": {
    "context7": {
      "url": "https://mcp.context7.com/mcp"
    }
  }
}
```

OAuth-backed MCP servers are covered in [MCP OAuth](mcp-oauth.md).

## Skills

The `skills` object can configure:

- `builtin`: allowlist of built-in tool names; omitted means all defaults
- `local`: explicit `SKILL.md` files
- `subagents`: child agents exposed as tools

Workspace-managed skills are also discovered from the workspace `skills/`
directory. See [Tools & Skills](tools-and-skills.md).

## Secret Values

Many fields accept a literal value or resolver object:

```json
{ "resolver": "insecure_env", "name": "ANTHROPIC_API_KEY" }
```

`$ENV_VAR` shorthand is supported in MCP environment maps. See
[Secrets](secrets.md) for the resolver system.
