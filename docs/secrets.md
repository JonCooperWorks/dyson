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

Every secret value in `dyson.toml` uses a `scheme:key` URI format:

```toml
[agent]
api_key = "insecure_env:ANTHROPIC_API_KEY"   # explicit: env var
api_key = "env:ANTHROPIC_API_KEY"            # shorthand alias
api_key = "$ANTHROPIC_API_KEY"               # $ shorthand → env:ANTHROPIC_API_KEY
api_key = "sk-ant-literal-value"             # literal (no scheme = no resolution)

[telegram]
bot_token = "insecure_env:TELEGRAM_BOT_TOKEN"   # from env
bot_token = "ssm:/dyson/telegram-token"          # from AWS SSM (future)
bot_token = "vault:secret/data/telegram"         # from Vault (future)
bot_token = "op:op://Personal/dyson/token"       # from 1Password (future)
```

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

The default resolver.  Reads from `std::env::var()`.

**The name is intentionally alarming.**  Environment variables are visible in
`/proc/*/environ`, in shell history, in CI logs, and inherited by every child
process the agent spawns.  This is fine for development.  In production, use
a real secret manager.

```rust
pub struct InsecureEnvironmentVariable;

impl SecretResolver for InsecureEnvironmentVariable {
    fn resolve(&self, key: &str) -> Result<String> {
        std::env::var(key)  // + error handling for empty/missing
    }
    fn scheme(&self) -> &str { "insecure_env" }
}
```

---

## Adding a New Resolver

1. Create `src/secret/my_resolver.rs`
2. Implement `SecretResolver`
3. Register it in `SecretRegistry::default()` or at startup

Example skeleton for AWS SSM:

```rust
pub struct AwsSsmResolver {
    client: aws_sdk_ssm::Client,
}

impl SecretResolver for AwsSsmResolver {
    fn resolve(&self, key: &str) -> Result<String> {
        // key is the SSM parameter path, e.g. "/dyson/api-key"
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async {
            self.client
                .get_parameter()
                .name(key)
                .with_decryption(true)
                .send()
                .await
                .map(|out| out.parameter().unwrap().value().unwrap().to_string())
                .map_err(|e| DysonError::Config(format!("SSM error: {e}")))
        })
    }

    fn scheme(&self) -> &str { "ssm" }
}
```

Then in config:
```toml
api_key = "ssm:/production/anthropic-api-key"
bot_token = "ssm:/production/telegram-bot-token"
```

---

## How Secrets Flow Through Config Loading

```
dyson.toml loaded
  │
  ▼
SecretRegistry::default()
  │  registers: "insecure_env" → InsecureEnvironmentVariable
  │  registers: "env" → InsecureEnvironmentVariable (alias)
  │
  ▼
build_settings(toml, &registry)
  │
  ├── agent.api_key = "env:ANTHROPIC_API_KEY"
  │     → registry.resolve_value("env:ANTHROPIC_API_KEY")
  │     → InsecureEnvironmentVariable.resolve("ANTHROPIC_API_KEY")
  │     → "sk-ant-..."
  │
  ├── telegram.bot_token = "$TELEGRAM_BOT_TOKEN"
  │     → registry.resolve_value("$TELEGRAM_BOT_TOKEN")
  │     → parse: scheme="env", key="TELEGRAM_BOT_TOKEN"
  │     → InsecureEnvironmentVariable.resolve("TELEGRAM_BOT_TOKEN")
  │     → "123456:ABC-DEF..."
  │
  └── agent.api_key = "" (not set in config)
        → resolve_with_env_fallback("", "ANTHROPIC_API_KEY")
        → try: resolvers["env"].resolve("ANTHROPIC_API_KEY")
        → std::env::var("ANTHROPIC_API_KEY")
```

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

---

See also: [Architecture Overview](architecture-overview.md) ·
[Configuration](configuration.md)
