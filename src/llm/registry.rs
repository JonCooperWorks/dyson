// ===========================================================================
// LLM Provider Registry — single source of truth for provider metadata.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Centralizes all per-provider metadata (aliases, default model, env var,
//   factory function) into a static registry.  Before this registry existed,
//   adding a new provider required touching 7 match arms across 4 files.
//   Now it's: add an enum variant, implement LlmClient, add a ProviderEntry.
//
// How it works:
//   `registry()` returns a static slice of `ProviderEntry` structs — one per
//   `LlmProvider` variant.  Helper functions (`lookup`, `from_str_loose`,
//   `all_canonical_names`) query the slice.  All other modules delegate to
//   these helpers instead of matching on `LlmProvider` directly.
//
// Why fn pointers instead of closures?
//   `ProviderEntry` lives in a `static` slice, which requires `Sync + 'static`.
//   `fn` pointers satisfy this; closures (even non-capturing) need `Box<dyn Fn>`
//   which can't live in a static.  None of our factories capture state, so
//   fn pointers work perfectly.
// ===========================================================================

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::auth::Credential;
use crate::config::LlmProvider;
use crate::error::{DysonError, Result};
use crate::llm::LlmClient;
use crate::secret::{SecretRegistry, SecretValue};
use crate::workspace::Workspace;

// ---------------------------------------------------------------------------
// ProviderEntry — one row in the registry.
// ---------------------------------------------------------------------------

/// Everything the system needs to know about a single LLM provider.
///
/// Each `LlmProvider` variant has exactly one `ProviderEntry` in the
/// registry.  A unit test enforces this 1:1 correspondence.
pub struct ProviderEntry {
    /// The enum variant this entry describes.
    pub provider: LlmProvider,

    /// Canonical name (used in display, serialization, error messages).
    pub canonical_name: &'static str,

    /// All recognized aliases for loose string parsing (must be lowercase).
    ///
    /// Used by `from_str_loose()` to map user input to `LlmProvider`.
    pub aliases: &'static [&'static str],

    /// Default model identifier when the user doesn't specify one.
    pub default_model: &'static str,

    /// Environment variable name for API key fallback.
    ///
    /// `None` for CLI-subprocess providers (ClaudeCode, Codex) that
    /// don't use API keys.
    pub env_var: Option<&'static str>,

    /// Whether this provider requires an API key to function.
    ///
    /// When `false`, the config loader skips env-var resolution and
    /// the custom-base_url security check for this provider.
    pub requires_api_key: bool,

    /// Factory function to construct the `LlmClient` implementation.
    ///
    /// Receives a `ClientConfig` with all possible inputs.  API-based
    /// clients use `api_key` and `base_url`; CLI-subprocess clients
    /// use `workspace` and `dangerous_no_sandbox`.
    pub create_client: fn(&ClientConfig) -> Box<dyn LlmClient>,
}

impl ProviderEntry {
    /// Resolve an API key for this provider, falling back to its env var.
    ///
    /// Encapsulates all provider-aware API key logic so the config loader
    /// doesn't need to know about individual providers:
    ///
    /// 1. If the provider doesn't need an API key → returns `Ok(None)`.
    /// 2. If `existing_key` is already populated → returns `Ok(None)` (no change).
    /// 3. If a custom `base_url` is set → blocks env-var fallback (security).
    /// 4. Otherwise → tries the provider's env var via `SecretRegistry`.
    ///
    /// Returns `Ok(Some(credential))` when a key was resolved, `Ok(None)`
    /// when no action is needed, or `Err` when a key is required but missing.
    pub fn resolve_api_key(
        &self,
        existing_key: &Credential,
        base_url: &Option<String>,
        secrets: &SecretRegistry,
        required: bool,
    ) -> Result<Option<Credential>> {
        if !self.requires_api_key {
            return Ok(None);
        }

        if !existing_key.is_empty() {
            return Ok(None);
        }

        // SECURITY: refuse to inject env-var keys when a custom base_url
        // is set — the key would be sent to an untrusted endpoint.
        if base_url.is_some() {
            if required {
                return Err(DysonError::Config(format!(
                    "provider has a custom base_url ({}) but no explicit api_key.  \
                     For security, environment-variable fallback is disabled when \
                     base_url is set — the key would be sent to a non-default endpoint.  \
                     Set the api_key explicitly in the provider config, or remove base_url \
                     to use the default API endpoint.",
                    base_url.as_deref().unwrap_or("?"),
                )));
            }
            return Ok(None);
        }

        let env_var = match self.env_var {
            Some(v) => v,
            None => return Ok(None),
        };

        match secrets.resolve_or_env_fallback(&SecretValue::Literal(String::new()), env_var) {
            Ok(key) => Ok(Some(key)),
            Err(e) => {
                if required {
                    Err(e)
                } else {
                    Ok(None)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ClientConfig — inputs to the factory function.
// ---------------------------------------------------------------------------

/// All inputs a factory function might need.
///
/// API-based clients ignore `workspace`/`dangerous_no_sandbox`;
/// CLI-subprocess clients ignore `api_key`.  This avoids needing
/// separate factory signatures for different provider patterns.
pub struct ClientConfig<'a> {
    /// API key (empty string for providers that don't need one).
    pub api_key: &'a str,

    /// Optional base URL override.
    pub base_url: Option<&'a str>,

    /// Shared workspace reference (used by CLI-subprocess providers).
    pub workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,

    /// Whether `--dangerous-no-sandbox` was passed (CLI-subprocess only).
    pub dangerous_no_sandbox: bool,
}

// ---------------------------------------------------------------------------
// The registry — static slice of all providers.
// ---------------------------------------------------------------------------

/// Returns the provider registry.
///
/// One entry per `LlmProvider` variant.  The unit test
/// `registry_covers_all_variants` ensures nothing is missing.
pub fn registry() -> &'static [ProviderEntry] {
    use super::{anthropic, claude_code, codex, ollama_cloud, openai, openai_compat, openrouter};

    static ENTRIES: std::sync::LazyLock<Vec<ProviderEntry>> = std::sync::LazyLock::new(|| {
        vec![
            ProviderEntry {
                provider: LlmProvider::Anthropic,
                canonical_name: "anthropic",
                aliases: &["anthropic"],
                default_model: "claude-sonnet-4-20250514",
                env_var: Some("ANTHROPIC_API_KEY"),
                requires_api_key: true,
                create_client: |c| Box::new(anthropic::AnthropicClient::new(c.api_key, c.base_url)),
            },
            ProviderEntry {
                provider: LlmProvider::OpenAi,
                canonical_name: "openai",
                aliases: &["openai", "gpt"],
                default_model: "gpt-4o",
                env_var: Some("OPENAI_API_KEY"),
                requires_api_key: true,
                create_client: |c| {
                    match c.base_url {
                        Some(url) if !url.starts_with("https://api.openai.com") => {
                            Box::new(openai_compat::OpenAiCompatClient::new(c.api_key, Some(url)))
                        }
                        _ => Box::new(openai::OpenAiClient::new(c.api_key, c.base_url)),
                    }
                },
            },
            ProviderEntry {
                provider: LlmProvider::OpenRouter,
                canonical_name: "openrouter",
                aliases: &["openrouter", "open-router", "open_router"],
                default_model: "anthropic/claude-sonnet-4",
                env_var: Some("OPENROUTER_API_KEY"),
                requires_api_key: true,
                create_client: |c| {
                    Box::new(openrouter::OpenRouterClient::new(c.api_key))
                },
            },
            ProviderEntry {
                provider: LlmProvider::ClaudeCode,
                canonical_name: "claude-code",
                aliases: &["claude-code", "claude_code", "cc"],
                default_model: "claude-sonnet-4-20250514",
                env_var: None,
                requires_api_key: false,
                create_client: |c| {
                    Box::new(claude_code::ClaudeCodeClient::new(
                        c.base_url,
                        vec![],
                        c.workspace.clone(),
                        c.dangerous_no_sandbox,
                    ))
                },
            },
            ProviderEntry {
                provider: LlmProvider::Codex,
                canonical_name: "codex",
                aliases: &["codex", "codex-cli"],
                default_model: "codex",
                env_var: None,
                requires_api_key: false,
                create_client: |c| {
                    Box::new(codex::CodexClient::new(
                        c.base_url,
                        c.workspace.clone(),
                        c.dangerous_no_sandbox,
                    ))
                },
            },
            ProviderEntry {
                provider: LlmProvider::OllamaCloud,
                canonical_name: "ollama-cloud",
                aliases: &["ollama-cloud", "ollama_cloud", "ollama"],
                default_model: "llama3.3",
                env_var: Some("OLLAMA_API_KEY"),
                requires_api_key: true,
                create_client: |c| {
                    Box::new(ollama_cloud::OllamaCloudClient::new(c.api_key))
                },
            },
        ]
    });

    &ENTRIES
}

// ---------------------------------------------------------------------------
// Lookup helpers
// ---------------------------------------------------------------------------

/// Look up a provider entry by enum variant.
///
/// Panics if the variant is missing from the registry (indicates a bug —
/// the unit test should catch this before it reaches production).
pub fn lookup(provider: &LlmProvider) -> &'static ProviderEntry {
    registry()
        .iter()
        .find(|e| e.provider == *provider)
        .expect("registry is missing a LlmProvider variant — add a ProviderEntry")
}

/// Parse a loose provider string (case-insensitive, with aliases).
///
/// Returns `None` for unrecognized strings.  This replaces the
/// `LlmProvider::from_str_loose` method body.
pub fn from_str_loose(s: &str) -> Option<LlmProvider> {
    let lower = s.to_lowercase();
    registry()
        .iter()
        .find(|e| e.aliases.iter().any(|a| *a == lower))
        .map(|e| e.provider.clone())
}

/// All canonical provider names, for use in error messages.
pub fn all_canonical_names() -> Vec<&'static str> {
    registry().iter().map(|e| e.canonical_name).collect()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_covers_all_variants() {
        // Every LlmProvider variant must have a registry entry.
        // If you add a variant and forget the entry, this test fails.
        let all_variants = [
            LlmProvider::Anthropic,
            LlmProvider::OpenAi,
            LlmProvider::OpenRouter,
            LlmProvider::ClaudeCode,
            LlmProvider::Codex,
            LlmProvider::OllamaCloud,
        ];
        assert_eq!(
            registry().len(),
            all_variants.len(),
            "registry length doesn't match LlmProvider variant count"
        );
        for variant in &all_variants {
            assert!(
                registry().iter().any(|e| e.provider == *variant),
                "registry is missing entry for {variant:?}"
            );
        }
    }

    #[test]
    fn lookup_finds_all_providers() {
        let entry = lookup(&LlmProvider::Anthropic);
        assert_eq!(entry.canonical_name, "anthropic");
        assert_eq!(entry.default_model, "claude-sonnet-4-20250514");
        assert_eq!(entry.env_var, Some("ANTHROPIC_API_KEY"));
        assert!(entry.requires_api_key);

        let entry = lookup(&LlmProvider::ClaudeCode);
        assert!(!entry.requires_api_key);
        assert_eq!(entry.env_var, None);
    }

    #[test]
    fn from_str_loose_matches_aliases() {
        assert_eq!(from_str_loose("anthropic"), Some(LlmProvider::Anthropic));
        assert_eq!(from_str_loose("OPENAI"), Some(LlmProvider::OpenAi));
        assert_eq!(from_str_loose("gpt"), Some(LlmProvider::OpenAi));
        assert_eq!(from_str_loose("open-router"), Some(LlmProvider::OpenRouter));
        assert_eq!(from_str_loose("cc"), Some(LlmProvider::ClaudeCode));
        assert_eq!(from_str_loose("codex-cli"), Some(LlmProvider::Codex));
        assert_eq!(from_str_loose("ollama-cloud"), Some(LlmProvider::OllamaCloud));
        assert_eq!(from_str_loose("ollama"), Some(LlmProvider::OllamaCloud));
        assert_eq!(from_str_loose("unknown"), None);
    }

    #[test]
    fn all_canonical_names_returns_all() {
        let names = all_canonical_names();
        assert!(names.contains(&"anthropic"));
        assert!(names.contains(&"openai"));
        assert!(names.contains(&"openrouter"));
        assert!(names.contains(&"claude-code"));
        assert!(names.contains(&"codex"));
        assert!(names.contains(&"ollama-cloud"));
    }

    #[test]
    fn no_duplicate_aliases() {
        let mut all_aliases = Vec::new();
        for entry in registry() {
            for alias in entry.aliases {
                assert!(
                    !all_aliases.contains(alias),
                    "duplicate alias '{alias}' in registry"
                );
                all_aliases.push(alias);
            }
        }
    }
}
