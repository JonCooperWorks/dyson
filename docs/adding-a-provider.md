# Adding an LLM Provider

How to add a new LLM provider to Dyson.  All provider knowledge lives in
`src/llm/` — the config loader doesn't know about individual providers.

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
    OllamaCloud,
    YourProvider,  // ← add here
}
```

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

That's it.  Nothing outside `src/llm/` needs to change.

## What the registry handles

The `ProviderEntry` is the single source of truth.  The config loader and
CLI interrogate it generically — they never match on `LlmProvider` variants.

| Field | What it drives |
|-------|---------------|
| `aliases` | Loose string parsing (`--provider openai`, `"type": "gpt"`) |
| `default_model` | Fallback when user doesn't specify a model |
| `env_var` | Environment variable for API key fallback |
| `requires_api_key` | Whether to attempt key resolution at all |
| `create_client` | Factory function called by the agent loop |
| `canonical_name` | Display in `--provider` error messages |

The `resolve_api_key()` method on `ProviderEntry` encapsulates the full
API key resolution flow (env var fallback, custom base_url security check),
so the config loader calls one method without knowing provider details.

## Testing

The `registry_covers_all_variants` test ensures every `LlmProvider` variant
has a corresponding registry entry.  Run the full suite:

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

```bash
# Via CLI flag
cargo run -- --provider your-provider --model your-model

# Via env var (if env_var is set in the registry entry)
export YOUR_PROVIDER_API_KEY="sk-..."
cargo run -- --provider your-provider
```
