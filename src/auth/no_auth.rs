// ===========================================================================
// NoAuth — pass-through authentication (no credentials).
//
// Used by:
//   - StdioTransport (auth is via env vars, not HTTP headers)
//   - ClaudeCode and Codex CLI providers (no API key needed)
// ===========================================================================

use async_trait::async_trait;

use crate::auth::{Auth, AuthInfo};
use crate::error::Result;

/// No-op authentication.
///
/// `apply_to_request` passes through unchanged.
/// `validate_request` accepts all requests as "anonymous".
pub struct NoAuth;

#[async_trait]
impl Auth for NoAuth {
    async fn apply_to_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder> {
        Ok(request)
    }

    async fn validate_request(&self, _headers: &hyper::HeaderMap) -> Result<AuthInfo> {
        Ok(AuthInfo::new("anonymous"))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn apply_passes_through() {
        let auth = NoAuth;
        let headers = super::super::test_apply(&auth).await;
        // No auth headers added.
        assert!(!headers.contains_key("authorization"));
        assert!(!headers.contains_key("x-api-key"));
    }

    #[tokio::test]
    async fn validate_accepts_all() {
        let auth = NoAuth;
        let headers = hyper::HeaderMap::new();
        let info = auth.validate_request(&headers).await.unwrap();
        assert_eq!(info.identity, "anonymous");
    }
}
