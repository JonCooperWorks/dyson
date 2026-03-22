# Adding an LLM Provider

How to add a new LLM provider to Dyson.  The provider registry
(`src/llm/registry.rs`) centralizes all per-provider metadata, so adding
a new provider is three steps.

## Steps

### 1. Add the enum variant

In `src/config/mod.rs`, add a variant to `LlmProvider`:

```rust
pub enum LlmProvider {
    Anthropic,
    OpenAi,
    OpenRouter,
    ClaudeCode,
    Codex,
    YourProvider,  // ← add here
}
```

The compiler will immediately flag any exhaustive matches you've missed,
but the registry handles all of them — so this is the only enum change needed.

### 2. Implement `LlmClient`

Create `src/llm/your_provider.rs` and implement the `LlmClient` trait:

```rust
#[async_trait]
impl LlmClient for YourProviderClient {
    async fn stream(
        &self,
        messages: &[Message],
        system: &str,
        tools: &[ToolDefinition],
        config: &CompletionConfig,
    ) -> Result<StreamResponse> {
        // ...
    }
}
```

Add `pub mod your_provider;` to `src/llm/mod.rs`.

**Two implementation patterns exist:**

| Pattern | Examples | Constructor signature |
|---------|----------|----------------------|
| **API-based** | Anthropic, OpenAI, OpenRouter | `new(api_key: &str, base_url: Option<&str>)` |
| **CLI subprocess** | ClaudeCode, Codex | `new(base_url: Option<&str>, workspace: Option<Arc<...>>, dangerous_no_sandbox: bool)` |

If your provider uses an OpenAI-compatible API, wrap `OpenAiClient` like
`OpenRouterClient` does — override the base URL and add any custom headers.

### 3. Register it

Add a `ProviderEntry` to the registry in `src/llm/registry.rs`:

```rust
ProviderEntry {
    provider: LlmProvider::YourProvider,
    canonical_name: "your-provider",
    aliases: &["your-provider", "yp"],
    default_model: "your-default-model",
    env_var: Some("YOUR_PROVIDER_API_KEY"),  // or None for CLI providers
    requires_api_key: true,                   // false for CLI providers
    create_client: |c| Box::new(
        your_provider::YourProviderClient::new(c.api_key, c.base_url)
    ),
},
```

That's it.  The registry drives:

- **String parsing** — `from_str_loose()` matches your aliases
- **Client creation** — `create_client()` calls your factory
- **Default model** — used when the user doesn't specify one
- **API key resolution** — env var fallback in the config loader
- **Error messages** — `--provider` help text lists all canonical names

## What the registry handles for you

| Concern | Before registry | After registry |
|---------|----------------|----------------|
| String aliases | `from_str_loose` match arm | `aliases` field |
| Default model | `loader.rs` match arm | `default_model` field |
| Env var name | Two `loader.rs` match arms | `env_var` field |
| Factory dispatch | `create_client` match arm | `create_client` field |
| Error message | Hardcoded string | `all_canonical_names()` |

## Testing

The `registry_covers_all_variants` test ensures every `LlmProvider` variant
has a corresponding registry entry.  If you add a variant but forget the
entry, this test fails.

Run the full test suite to verify:

```bash
cargo test
```

## Configuration

Once registered, your provider works everywhere:

```json
{
  "providers": {
    "my-provider": {
      "type": "your-provider",
      "model": "your-model",
      "api_key": "sk-..."
    }
  }
}
```

Or via CLI:

```bash
cargo run -- --provider your-provider --model your-model
```

Or via env var (if `env_var` is set):

```bash
export YOUR_PROVIDER_API_KEY="sk-..."
cargo run -- --provider your-provider
```
