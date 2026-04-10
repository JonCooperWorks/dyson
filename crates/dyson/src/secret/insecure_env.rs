// ===========================================================================
// InsecureEnvironmentVariable — resolve secrets from environment variables.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements the `SecretResolver` trait by reading secrets from
//   environment variables via `std::env::var()`.  This is the default
//   resolver — the one that ships with Dyson and requires zero setup.
//
// Why "Insecure" in the name?
//   Environment variables are the least secure place to store secrets:
//
//   - Visible in `/proc/<pid>/environ` on Linux (any process as the
//     same user can read them)
//   - Logged in shell history (`export API_KEY=sk-...`)
//   - Leaked in CI build logs, Docker inspect output, crash reports
//   - Inherited by child processes (every `bash -c` Dyson spawns gets
//     ALL environment variables, including secrets)
//   - Not encrypted at rest or in transit
//
//   The name is intentionally alarming.  When you see
//   `InsecureEnvironmentVariable` in your config or logs, it's a reminder
//   that you should migrate to a real secret manager before production.
//
// What to use instead (future resolvers):
//
//   | Resolver | Backend | Security level |
//   |----------|---------|---------------|
//   | InsecureEnvironmentVariable | `std::env::var()` | Development only |
//   | (future) VaultResolver | HashiCorp Vault | Production |
//   | (future) AwsSsmResolver | AWS SSM Parameter Store | Production |
//   | (future) OpResolver | 1Password CLI (`op read`) | Personal/Team |
//   | (future) GcpResolver | GCP Secret Manager | Production |
//   | (future) InfisicalResolver | Infisical | Team |
//
// How it's used:
//
//   This resolver does NO parsing.  It receives a raw key string (the
//   `name` field from the config) and calls `std::env::var(key)`.
//   The config layer handles deserialization and resolver dispatch —
//   this resolver just does the lookup.
//
//   The flow:
//     dyson.json:  "api_key": { "resolver": "insecure_env", "name": "ANTHROPIC_API_KEY" }
//       → serde deserializes as SecretValue::Reference
//       → SecretRegistry.resolve() finds "insecure_env" → this resolver
//       → calls resolve("ANTHROPIC_API_KEY")
//       → std::env::var("ANTHROPIC_API_KEY")
//       → "sk-ant-..."
//
//   Config examples:
//
//   ```json
//   {
//     "agent": {
//       // Resolver form:
//       "api_key": { "resolver": "insecure_env", "name": "ANTHROPIC_API_KEY" }
//     }
//   }
//   ```
//
//   ```json
//   {
//     "agent": {
//       // Literal form (no resolver, value used as-is):
//       "api_key": "sk-ant-literal-value"
//     }
//   }
//   ```
// ===========================================================================

use crate::error::{DysonError, Result};
use crate::secret::SecretResolver;

// ---------------------------------------------------------------------------
// InsecureEnvironmentVariable
// ---------------------------------------------------------------------------

/// Resolves secrets from environment variables.
///
/// This resolver does no parsing.  It receives a raw key string (already
/// extracted by the `SecretRegistry`) and calls `std::env::var(key)`.
/// It either returns the value or an error — nothing else.
///
/// ## Why not just call std::env::var() directly?
///
/// Two reasons:
/// 1. **Uniformity** — all secrets flow through the same code path,
///    making it easy to audit and replace.  `grep -r "SecretResolver"`
///    shows you every place secrets are accessed.
/// 2. **Error context** — the error message includes the key name and
///    a hint on how to fix it.
pub struct InsecureEnvironmentVariable;

#[allow(clippy::needless_lifetimes)]
impl SecretResolver for InsecureEnvironmentVariable {
    /// Look up a secret in the process environment.
    ///
    /// ## Errors
    ///
    /// - Variable not set → clear error message with `export` hint
    /// - Variable set but empty → separate error (catches `export KEY=""`)
    fn resolve(&self, key: &str) -> Result<String> {
        match std::env::var(key) {
            Ok(val) if !val.is_empty() => {
                tracing::trace!(key = key, "secret resolved from environment");
                Ok(val)
            }
            Ok(_) => Err(DysonError::Config(format!(
                "secret '{key}' is set but empty (insecure_env resolver).  \
                 Did you mean to set a value?  export {key}=<value>"
            ))),
            Err(_) => Err(DysonError::Config(format!(
                "secret '{key}' not found in environment variables.  \
                 Set it with: export {key}=<value>"
            ))),
        }
    }

    fn scheme(&self) -> &str {
        "insecure_env"
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_set_var() {
        unsafe { std::env::set_var("DYSON_INSECURE_TEST_1", "value123") };
        let resolver = InsecureEnvironmentVariable;
        assert_eq!(
            resolver.resolve("DYSON_INSECURE_TEST_1").unwrap(),
            "value123"
        );
        unsafe { std::env::remove_var("DYSON_INSECURE_TEST_1") };
    }

    #[test]
    fn errors_on_missing_var() {
        let resolver = InsecureEnvironmentVariable;
        let err = resolver
            .resolve("DYSON_DEFINITELY_NOT_SET_99999")
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
        assert!(err.to_string().contains("export"));
    }

    #[test]
    fn errors_on_empty_var() {
        unsafe { std::env::set_var("DYSON_INSECURE_TEST_2", "") };
        let resolver = InsecureEnvironmentVariable;
        let err = resolver.resolve("DYSON_INSECURE_TEST_2").unwrap_err();
        assert!(err.to_string().contains("empty"));
        unsafe { std::env::remove_var("DYSON_INSECURE_TEST_2") };
    }

    #[test]
    fn scheme_is_insecure_env() {
        let resolver = InsecureEnvironmentVariable;
        assert_eq!(resolver.scheme(), "insecure_env");
    }
}
