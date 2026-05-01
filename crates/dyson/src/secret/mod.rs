// ===========================================================================
// Secret resolution — per-secret, explicit routing to multiple backends.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Defines the `SecretResolver` trait, a `SecretRegistry` of resolvers,
//   and a `SecretValue` type that deserializes from JSON as either a
//   literal string or an explicit { resolver, name } object.
//
// Design principle: no magic, no parsing.
//   A secret in dyson.json is one of two things:
//
//   1. A literal string:
//      ```json
//      "api_key": "sk-ant-literal-value"
//      ```
//
//   2. An object naming the resolver and key:
//      ```json
//      "api_key": { "resolver": "insecure_env", "name": "ANTHROPIC_API_KEY" }
//      ```
//
//   There is no prefix parsing, no $-shorthand, no scheme:key URI syntax.
//   The resolver field explicitly names the backend.  The name field is the
//   raw key passed to that backend.  No transformation, no stripping.
//
// Per-secret routing:
//   Each secret can use a different backend:
//
//   ```json
//   {
//     "agent": {
//       "api_key": { "resolver": "insecure_env", "name": "ANTHROPIC_API_KEY" }
//     },
//     "controllers": [
//       {
//         "type": "my_bot",
//         "api_key": { "resolver": "insecure_env", "name": "BOT_API_KEY" }
//       }
//     ]
//   }
//   ```
//
//   Future resolvers (vault, ssm, op) plug in the same way — just a
//   different string in the `resolver` field.
//
// SecretValue deserialization:
//   Serde's `#[serde(untagged)]` enum lets TOML values be either a string
//   or a table transparently:
//
//   ```rust
//   enum SecretValue {
//       Literal(String),                              // api_key = "sk-ant-..."
//       Reference { resolver: String, name: String },  // api_key = { resolver = "...", name = "..." }
//   }
//   ```
//
//   The config layer deserializes into SecretValue, then calls
//   `registry.resolve(&secret_value)` which either returns the literal
//   or delegates to the named resolver.
// ===========================================================================

pub mod insecure_env;

pub use insecure_env::InsecureEnvironmentVariable;

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;

use crate::error::{DysonError, Result};

// ---------------------------------------------------------------------------
// SecretResolver trait
// ---------------------------------------------------------------------------

/// A single backend for resolving secret values.
///
/// Each resolver handles one type of secret storage.  The resolver receives
/// a raw key string and returns the secret value.  It does no parsing —
/// the key is exactly what was written in the `name` field of the config.
///
/// ## Implementing a custom resolver
///
/// ```ignore
/// struct VaultResolver { client: vault::Client }
///
/// impl SecretResolver for VaultResolver {
///     fn resolve(&self, key: &str) -> Result<String> {
///         self.client.read_secret(key)
///             .map_err(|e| DysonError::Config(format!("vault: {e}")))
///     }
///     fn scheme(&self) -> &str { "vault" }
/// }
/// ```
///
/// Register it and use in config:
/// ```json
/// "api_key": { "resolver": "vault", "name": "secret/data/anthropic-key" }
/// ```
pub trait SecretResolver: Send + Sync {
    /// Resolve a secret by its key.
    ///
    /// `key` is the raw `name` field from the config — no transformation,
    /// no prefix stripping.  For insecure_env, this is an environment
    /// variable name like `"ANTHROPIC_API_KEY"`.
    fn resolve(&self, key: &str) -> Result<String>;

    /// The name this resolver is registered under (e.g., "insecure_env").
    fn scheme(&self) -> &str;
}

// ---------------------------------------------------------------------------
// SecretValue — what a secret looks like in the config file.
// ---------------------------------------------------------------------------

/// A config value that is either a literal string or a reference to a
/// secret in an external backend.
///
/// JSON deserialization handles both forms transparently:
///
/// ```json
/// // Literal — the value IS the secret:
/// "api_key": "sk-ant-literal-value"
///
/// // Reference — fetch from a backend:
/// "api_key": { "resolver": "insecure_env", "name": "ANTHROPIC_API_KEY" }
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SecretValue {
    /// A literal string value — used as-is, no resolution.
    Literal(String),

    /// A reference to a secret in an external backend.
    Reference {
        /// Which resolver to use (e.g., "insecure_env", "vault", "ssm").
        resolver: String,
        /// The raw key passed to the resolver.  No transformation.
        name: String,
    },
}

impl SecretValue {
    /// Returns true if this is an empty literal (no value configured).
    pub const fn is_empty(&self) -> bool {
        match self {
            Self::Literal(s) => s.is_empty(),
            Self::Reference { name, .. } => name.is_empty(),
        }
    }
}

// ---------------------------------------------------------------------------
// SecretRegistry — holds resolvers and resolves SecretValues.
// ---------------------------------------------------------------------------

/// Registry of secret resolvers, keyed by name.
///
/// The registry is built once at startup with all available resolvers.
/// Config loading calls `registry.resolve(&secret_value)` for each
/// secret, which either returns the literal or delegates to the named
/// resolver.
///
/// ## Example
///
/// ```ignore
/// let registry = SecretRegistry::default(); // has "insecure_env"
///
/// // Literal — returned as-is
/// let val = registry.resolve(&SecretValue::Literal("sk-ant-...".into()))?;
/// assert_eq!(val, "sk-ant-...");
///
/// // Reference — delegates to insecure_env resolver
/// let val = registry.resolve(&SecretValue::Reference {
///     resolver: "insecure_env".into(),
///     name: "ANTHROPIC_API_KEY".into(),
/// })?;
/// // → std::env::var("ANTHROPIC_API_KEY")
/// ```
pub struct SecretRegistry {
    resolvers: HashMap<String, Arc<dyn SecretResolver>>,
}

impl SecretRegistry {
    pub fn new() -> Self {
        Self {
            resolvers: HashMap::new(),
        }
    }

    /// Register a resolver under a name.
    pub fn register(&mut self, name: &str, resolver: Arc<dyn SecretResolver>) {
        self.resolvers.insert(name.to_string(), resolver);
    }

    /// Resolve a SecretValue to a `Credential`.
    ///
    /// - `Literal(s)` → wraps `s` in a `Credential`
    /// - `Reference { resolver, name }` → looks up the resolver, calls
    ///   `resolver.resolve(name)`, wraps the result in a `Credential`
    ///
    /// The returned `Credential` zeroizes the secret from memory on drop.
    pub fn resolve(&self, value: &SecretValue) -> Result<crate::auth::Credential> {
        let raw = match value {
            SecretValue::Literal(s) => s.clone(),
            SecretValue::Reference { resolver, name } => {
                let r = self.resolvers.get(resolver.as_str()).ok_or_else(|| {
                    let available: Vec<&str> = self
                        .resolvers
                        .keys()
                        .map(std::string::String::as_str)
                        .collect();
                    DysonError::Config(format!(
                        "unknown secret resolver '{resolver}'.  \
                         Available: [{}].",
                        available.join(", ")
                    ))
                })?;

                tracing::trace!(resolver = resolver, name = name, "resolving secret");

                r.resolve(name)?
            }
        };
        Ok(crate::auth::Credential::new(raw))
    }

    /// Resolve a SecretValue, falling back to an env var if the value is
    /// an empty literal.
    ///
    /// Used for API keys where we want `ANTHROPIC_API_KEY` as a default
    /// when no explicit config is provided.  Returns a `Credential`.
    pub fn resolve_or_env_fallback(
        &self,
        value: &SecretValue,
        env_fallback: &str,
    ) -> Result<crate::auth::Credential> {
        if !value.is_empty() {
            return self.resolve(value);
        }

        // Try the env fallback via the insecure_env resolver.
        if let Some(env_resolver) = self.resolvers.get("insecure_env")
            && let Ok(val) = env_resolver.resolve(env_fallback)
        {
            return Ok(crate::auth::Credential::new(val));
        }

        Err(DysonError::Config(format!(
            "no value configured and {env_fallback} not set in environment.  \
             Set it with: export {env_fallback}=<value>  \
             Or in dyson.json: \"api_key\": {{ \"resolver\": \"insecure_env\", \"name\": \"{env_fallback}\" }}"
        )))
    }
}

impl Default for SecretRegistry {
    /// Creates a registry with `InsecureEnvironmentVariable` registered
    /// under `"insecure_env"`.
    fn default() -> Self {
        let mut registry = Self::new();
        registry.register("insecure_env", Arc::new(InsecureEnvironmentVariable));
        registry
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_literal() {
        let registry = SecretRegistry::default();
        let val = registry
            .resolve(&SecretValue::Literal("literal-value".into()))
            .unwrap();
        assert_eq!(val, "literal-value");
    }

    #[test]
    fn resolve_reference() {
        unsafe { std::env::set_var("DYSON_SECRET_TEST_1", "resolved") };
        let registry = SecretRegistry::default();
        let val = registry
            .resolve(&SecretValue::Reference {
                resolver: "insecure_env".into(),
                name: "DYSON_SECRET_TEST_1".into(),
            })
            .unwrap();
        assert_eq!(val, "resolved");
        unsafe { std::env::remove_var("DYSON_SECRET_TEST_1") };
    }

    #[test]
    fn resolve_unknown_resolver_errors() {
        let registry = SecretRegistry::default();
        let err = registry
            .resolve(&SecretValue::Reference {
                resolver: "vault".into(),
                name: "secret/key".into(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("unknown secret resolver 'vault'"));
    }

    #[test]
    fn resolve_missing_env_var_errors() {
        let registry = SecretRegistry::default();
        let err = registry
            .resolve(&SecretValue::Reference {
                resolver: "insecure_env".into(),
                name: "DYSON_NOT_SET_99999".into(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn resolve_or_fallback_uses_explicit() {
        unsafe { std::env::set_var("DYSON_SECRET_TEST_2", "explicit") };
        let registry = SecretRegistry::default();
        let val = registry
            .resolve_or_env_fallback(
                &SecretValue::Reference {
                    resolver: "insecure_env".into(),
                    name: "DYSON_SECRET_TEST_2".into(),
                },
                "UNUSED",
            )
            .unwrap();
        assert_eq!(val, "explicit");
        unsafe { std::env::remove_var("DYSON_SECRET_TEST_2") };
    }

    #[test]
    fn resolve_or_fallback_uses_env() {
        unsafe { std::env::set_var("DYSON_SECRET_FALLBACK", "from_env") };
        let registry = SecretRegistry::default();
        let val = registry
            .resolve_or_env_fallback(
                &SecretValue::Literal(String::new()),
                "DYSON_SECRET_FALLBACK",
            )
            .unwrap();
        assert_eq!(val, "from_env");
        unsafe { std::env::remove_var("DYSON_SECRET_FALLBACK") };
    }

    #[test]
    fn deserialize_literal() {
        let val: SecretValue = serde_json::from_str(r#""literal""#).unwrap();
        assert!(matches!(val, SecretValue::Literal(s) if s == "literal"));
    }

    #[test]
    fn deserialize_reference() {
        let val: SecretValue =
            serde_json::from_str(r#"{"resolver":"insecure_env","name":"MY_KEY"}"#).unwrap();
        match val {
            SecretValue::Reference { resolver, name } => {
                assert_eq!(resolver, "insecure_env");
                assert_eq!(name, "MY_KEY");
            }
            _ => panic!("expected Reference"),
        }
    }

    #[test]
    fn custom_resolver() {
        struct MockResolver;
        impl SecretResolver for MockResolver {
            fn resolve(&self, key: &str) -> Result<String> {
                Ok(format!("mock:{key}"))
            }
            fn scheme(&self) -> &str {
                "mock"
            }
        }

        let mut registry = SecretRegistry::default();
        registry.register("mock", Arc::new(MockResolver));

        let val = registry
            .resolve(&SecretValue::Reference {
                resolver: "mock".into(),
                name: "my_secret".into(),
            })
            .unwrap();
        assert_eq!(val, "mock:my_secret");
    }
}
