// ===========================================================================
// Authentication — composable auth for all HTTP boundaries.
//
// LEARNING OVERVIEW
//
// What this module does:
//   Provides a single `Auth` trait that covers every auth boundary in Dyson:
//
//   1. Client-side (outgoing requests):
//      - LLM API calls: Anthropic `x-api-key`, OpenAI `Authorization: Bearer`
//      - MCP client: custom headers to external MCP servers
//
//   2. Server-side (incoming requests):
//      - MCP server: bearer token validation for Claude Code
//
//   3. Composability:
//      - `CompositeAuth`: chain multiple auth layers
//
// Design principle: one trait, two directions.
//   `apply_to_request()` adds credentials to outgoing requests.
//   `validate_request()` checks credentials on incoming requests.
//   Both have default impls so you only override the direction you need.
//
// Why not reuse SecretResolver?
//   SecretResolver resolves a key *name* to a secret *value* (string).
//   Auth applies that value to an HTTP request in a protocol-specific way
//   (header name, format, position).  They are complementary layers:
//   config loads secrets via SecretResolver, then passes them to Auth.
//
// Composability:
//
//   CompositeAuth::new(vec![
//       Box::new(BearerTokenAuth::new(key)),
//       Box::new(StaticHeadersAuth::new(extra)),
//   ])
//
// Memory safety:
//   All auth types that hold secrets (BearerTokenAuth, ApiKeyAuth) implement
//   `Zeroize + Drop` to clear sensitive data from memory when no longer needed.
// ===========================================================================

pub mod api_key;
pub mod bearer;
pub mod composite;
pub mod credential;
pub mod no_auth;
pub mod oauth;
pub mod static_headers;


pub use api_key::ApiKeyAuth;
pub use bearer::BearerTokenAuth;
pub use composite::CompositeAuth;
pub use credential::Credential;
pub use no_auth::NoAuth;
pub use oauth::OAuth;
pub use static_headers::StaticHeadersAuth;


use std::collections::HashMap;

use async_trait::async_trait;

use crate::error::{DysonError, Result};

// ---------------------------------------------------------------------------
// AuthInfo — metadata about an authenticated request.
// ---------------------------------------------------------------------------

/// Metadata returned by successful authentication.
///
/// Available to downstream code (audit logging, access control, etc.).
/// The `identity` field identifies the auth scheme; `metadata` carries
/// arbitrary key-value pairs for audit trails.
#[derive(Debug, Clone)]
pub struct AuthInfo {
    /// Identifies the auth scheme (e.g., "bearer", "api-key:x-api-key", "anonymous").
    pub identity: String,

    /// Arbitrary metadata for audit logging.
    pub metadata: HashMap<String, String>,
}

impl AuthInfo {
    pub fn new(identity: impl Into<String>) -> Self {
        Self {
            identity: identity.into(),
            metadata: HashMap::new(),
        }
    }

    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

// ---------------------------------------------------------------------------
// Auth trait
// ---------------------------------------------------------------------------

/// Composable authentication for both client-side and server-side HTTP.
///
/// Implementors override one or both methods depending on their role:
///
/// - **Client auth** (ApiKeyAuth, BearerTokenAuth): implements `apply_to_request`
/// - **Server auth** (BearerTokenAuth): implements `validate_request`
/// - **Composable wrappers** (CompositeAuth): chains multiple auth layers
///
/// ## Client-side example
///
/// ```ignore
/// let auth = ApiKeyAuth::new("x-api-key", api_key);
/// let req = client.post(url);
/// let req = auth.apply_to_request(req).await?;
/// let response = req.send().await?;
/// ```
///
/// ## Server-side example
///
/// ```ignore
/// match auth.validate_request(req.headers()).await {
///     Ok(info) => { /* proceed, info.identity has the caller */ }
///     Err(_) => { return unauthorized_response(); }
/// }
/// ```
#[async_trait]
pub trait Auth: Send + Sync {
    /// Apply authentication to an outgoing HTTP request.
    ///
    /// Takes ownership of the `RequestBuilder` and returns a new one with
    /// credentials applied.  This naturally supports chaining because
    /// reqwest's `.header()` consumes and returns the builder.
    ///
    /// The default is a no-op pass-through.
    async fn apply_to_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder> {
        Ok(request)
    }

    /// Validate authentication on an incoming HTTP request.
    ///
    /// Returns `Ok(AuthInfo)` if the request is authenticated, or `Err`
    /// if it should be rejected.
    ///
    /// The default rejects all requests (secure by default).
    async fn validate_request(&self, headers: &hyper::HeaderMap) -> Result<AuthInfo> {
        let _ = headers;
        Err(DysonError::Config(
            "auth: validate_request not implemented".into(),
        ))
    }

    /// Called when a request receives a 401 Unauthorized response.
    ///
    /// Gives the auth implementation a chance to refresh credentials before
    /// the caller retries the request.  Used by `OAuth` to force-refresh
    /// an access token that the server rejected (clock skew, revocation, etc.).
    ///
    /// The default is a no-op (most auth types have static credentials).
    async fn on_unauthorized(&self) -> Result<()> {
        Ok(())
    }
}

/// Constant-time byte comparison that also checks length.
///
/// Uses `subtle::ConstantTimeEq` to prevent timing side-channels when
/// comparing secrets (API keys, bearer tokens).
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

// ===========================================================================
// Tests
// ===========================================================================

/// Test helper: apply auth to a dummy request and return the resulting headers.
///
/// Avoids repeating `Client::new() → post → apply → build → headers` in every
/// auth test.
#[cfg(test)]
pub(crate) async fn test_apply(auth: &dyn Auth) -> reqwest::header::HeaderMap {
    let req = crate::http::client().post("http://localhost/test");
    let req = auth.apply_to_request(req).await.unwrap();
    req.build().unwrap().headers().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_info_builder() {
        let info = AuthInfo::new("bearer")
            .with_metadata("client", "claude-code")
            .with_metadata("port", "8080");

        assert_eq!(info.identity, "bearer");
        assert_eq!(info.metadata.get("client").unwrap(), "claude-code");
        assert_eq!(info.metadata.get("port").unwrap(), "8080");
    }

    #[tokio::test]
    async fn default_validate_rejects() {
        struct DefaultAuth;
        impl DefaultAuth {
            fn new() -> Self {
                Self
            }
        }

        #[async_trait]
        impl Auth for DefaultAuth {}

        let auth = DefaultAuth::new();
        let headers = hyper::HeaderMap::new();
        assert!(auth.validate_request(&headers).await.is_err());
    }
}
