# Secrets

Dyson resolves secrets through a per-secret, scheme-based routing system.
Each secret declares its own backend — one key can come from an environment
variable, another from AWS SSM, a third from Vault.  All in the same config
file.

**Key files:**
- `src/secret/mod.rs` — `SecretResolver` trait, `SecretRegistry`, `parse_secret_ref()`
- `src/secret/insecure_env.rs` — `InsecureEnvironmentVariable` (reads from `std::env::var()`)

---

## Secret Reference Syntax

Every secret value in `dyson.json` can be a literal string or a resolver
reference.  The JSON config uses `serde(untagged)` deserialization, so both
forms work:

```json
{
  "providers": {
    "claude": {
      "type": "anthropic",
      "api_key": "sk-ant-literal-value"
    },
    "gpt": {
      "type": "openai",
      "api_key": { "resolver": "insecure_env", "name": "OPENAI_API_KEY" }
    }
  },
  "controllers": [
    {
      "type": "telegram",
      "bot_token": { "resolver": "insecure_env", "name": "TELEGRAM_BOT_TOKEN" }
    }
  ]
}
```

Environment variables are also resolved automatically for API-based providers
when no explicit key is set (falls back to `ANTHROPIC_API_KEY` or `OPENAI_API_KEY`).

### Parsing rules

| Input | Scheme | Key | Resolved by |
|-------|--------|-----|-------------|
| `"insecure_env:API_KEY"` | `insecure_env` | `API_KEY` | `InsecureEnvironmentVariable` |
| `"env:API_KEY"` | `env` | `API_KEY` | `InsecureEnvironmentVariable` (alias) |
| `"$API_KEY"` | `env` | `API_KEY` | `InsecureEnvironmentVariable` (shorthand) |
| `"ssm:/path/key"` | `ssm` | `/path/key` | (future) AWS SSM |
| `"vault:secret/data/key"` | `vault` | `secret/data/key` | (future) Vault |
| `"sk-ant-literal"` | (none) | — | Returned as-is |

**Scheme detection**: the part before the first `:` must be purely lowercase
letters (`[a-z]+`).  This prevents false positives — values like
`"sk-ant-api03:rest"` (hyphens/digits) are treated as literals.

---

## SecretResolver Trait

```rust
pub trait SecretResolver: Send + Sync {
    fn resolve(&self, key: &str) -> Result<String>;
    fn scheme(&self) -> &str;
}
```

| Method | Purpose |
|--------|---------|
| `resolve(key)` | Look up a secret value by its key |
| `scheme()` | The URI scheme this resolver handles |

---

## SecretRegistry

The registry holds multiple resolvers and routes each secret reference to
the right one:

```rust
pub struct SecretRegistry {
    resolvers: HashMap<String, Arc<dyn SecretResolver>>,
}
```

```
registry.resolve_value("env:API_KEY")
  → parse: scheme="env", key="API_KEY"
  → lookup: resolvers["env"] = InsecureEnvironmentVariable
  → call: resolve("API_KEY")
  → std::env::var("API_KEY") → "sk-ant-..."

registry.resolve_value("ssm:/dyson/token")
  → parse: scheme="ssm", key="/dyson/token"
  → lookup: resolvers["ssm"] = AwsSsmResolver
  → call: resolve("/dyson/token")
  → AWS SSM GetParameter → "bot-token-value"

registry.resolve_value("literal-value")
  → parse: no scheme detected
  → return "literal-value" as-is
```

### Default registry

`SecretRegistry::default()` registers `InsecureEnvironmentVariable` under
two schemes:

| Scheme | Why |
|--------|-----|
| `insecure_env` | Canonical name — makes the security posture explicit |
| `env` | Shorthand alias — makes `$VAR` and `env:VAR` work |

---

## InsecureEnvironmentVariable

The default resolver — reads from `std::env::var()`. The name is intentionally alarming: env vars are visible in `/proc/*/environ`, shell history, CI logs, and inherited by child processes. Fine for development; use a real secret manager in production.

---

## Adding a New Resolver

1. Create `src/secret/my_resolver.rs`
2. Implement `SecretResolver`
3. Register it in `SecretRegistry::default()` or at startup

Implement `SecretResolver` (two methods: `resolve(key)` and `scheme()`), then register in `SecretRegistry::default()`. Config usage: `"api_key": "ssm:/production/anthropic-api-key"`.

---

## Config Loading Flow

At load time, `build_settings()` walks the JSON tree and resolves each `{ "resolver": ..., "name": ... }` object via the `SecretRegistry`. For API-based providers, missing keys fall back to the provider-specific env var (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`).

---

## Security Considerations

| Resolver | Risk | Mitigation |
|----------|------|------------|
| `insecure_env` | Secrets in process environment, visible to child processes | Dev only. Use a real secret manager in prod |
| (future) `ssm` | Requires IAM permissions, network access | Standard AWS IAM policies |
| (future) `vault` | Requires Vault token | Token rotation, AppRole auth |
| (future) `op` | Requires 1Password CLI auth | Biometric unlock, session timeout |

The scheme name `insecure_env` is deliberately uncomfortable.  When you see
it in your config, it's a reminder to migrate.

### Zeroize on drop

API key strings in `AnthropicClient` and `OpenAiClient` are zeroized when
the client is dropped (via the `zeroize` crate).  The MCP server's
per-session bearer token is also zeroized on drop.  This prevents secrets
from lingering in freed memory.

### MCP bearer token

The in-process MCP server generates a per-session bearer token (64 hex chars)
that is passed to Claude Code via CLI args.  The token is **not** in shell
history (subprocess is spawned programmatically) but **is** visible in `ps`
output while the process runs.  It's ephemeral (new token per LLM turn) and
only usable on loopback.  See [Tool Forwarding over MCP](tool-forwarding-over-mcp.md)
for details.

---

See also: [Architecture Overview](architecture-overview.md) ·
[Configuration](configuration.md)
