// ===========================================================================
// DangerousNoAuth — explicit opt-in to skip authentication.
//
// Pairs with transports that already delegate auth to another layer (stdio
// env vars, CLI providers like ClaudeCode/Codex) AND with HTTP boundaries
// that an operator has deliberately chosen to run unauthenticated.  The
// `Dangerous` prefix mirrors `--dangerous-no-sandbox` — it is the escape
// hatch, not a fallback, and callers must name it in config to use it.
// ===========================================================================

use async_trait::async_trait;

use crate::auth::{Auth, AuthInfo};
use crate::error::Result;

/// No-op authentication.  Accepts every incoming request as `"anonymous"`
/// and adds no credentials to outgoing requests.
pub struct DangerousNoAuth;

#[async_trait]
impl Auth for DangerousNoAuth {
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
        let auth = DangerousNoAuth;
        let headers = super::super::test_apply(&auth).await;
        // No auth headers added.
        assert!(!headers.contains_key("authorization"));
        assert!(!headers.contains_key("x-api-key"));
    }

    #[tokio::test]
    async fn validate_accepts_all() {
        let auth = DangerousNoAuth;
        let headers = hyper::HeaderMap::new();
        let info = auth.validate_request(&headers).await.unwrap();
        assert_eq!(info.identity, "anonymous");
    }
}
