// ===========================================================================
// BearerTokenAuth — `Authorization: Bearer <token>` for both directions.
//
// Used by:
//   - OpenAI client (client-side: sends Bearer token with API calls)
//   - MCP server (server-side: validates Bearer token from Claude Code)
//   - MCP config (token passed to Claude Code via --mcp-config headers)
//
// Token generation:
//   `generate()` creates a 64 hex-char token from 32 bytes of CSPRNG output
//   (rand::rngs::OsRng via thread_rng).  Used by the MCP server to create a
//   per-session token.
//
// Memory safety:
//   Uses `Credential` internally — the token is zeroized on drop.
// ===========================================================================

use async_trait::async_trait;
use rand::RngExt;

use crate::auth::{Auth, AuthInfo, Credential};
use crate::error::{DysonError, Result};

/// Bearer token authentication.
///
/// Client-side: adds `Authorization: Bearer <token>` to outgoing requests.
/// Server-side: validates the same header on incoming requests.
pub struct BearerTokenAuth {
    token: Credential,
}

impl BearerTokenAuth {
    /// Create from an existing token string.
    pub fn new(token: String) -> Self {
        Self {
            token: Credential::new(token),
        }
    }

    /// Generate a random bearer token (64 hex chars from 32 bytes of CSPRNG).
    ///
    /// Used by the MCP server to create a per-session token that Claude Code
    /// includes in its requests.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::rng().fill(&mut bytes);
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Self {
            token: Credential::new(token),
        }
    }

    /// Returns the raw token string.
    ///
    /// Needed by the MCP server to pass the token to Claude Code via
    /// `--mcp-config` headers.
    pub fn token(&self) -> &str {
        self.token.expose()
    }
}

#[async_trait]
impl Auth for BearerTokenAuth {
    async fn apply_to_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder> {
        Ok(request.header("Authorization", format!("Bearer {}", self.token.expose())))
    }

    async fn validate_request(&self, headers: &hyper::HeaderMap) -> Result<AuthInfo> {
        let valid = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| t == self.token.expose())
            .unwrap_or(false);

        if valid {
            Ok(AuthInfo::new("bearer"))
        } else {
            Err(DysonError::Config("unauthorized".into()))
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_produces_64_hex_chars() {
        let auth = BearerTokenAuth::generate();
        assert_eq!(auth.token().len(), 64);
        assert!(auth.token().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_produces_unique_tokens() {
        let a = BearerTokenAuth::generate();
        let b = BearerTokenAuth::generate();
        assert_ne!(a.token(), b.token());
    }

    #[tokio::test]
    async fn validate_accepts_matching_token() {
        let auth = BearerTokenAuth::new("test-token-123".into());

        let mut headers = hyper::HeaderMap::new();
        headers.insert("authorization", "Bearer test-token-123".parse().unwrap());

        let info = auth.validate_request(&headers).await.unwrap();
        assert_eq!(info.identity, "bearer");
    }

    #[tokio::test]
    async fn validate_rejects_wrong_token() {
        let auth = BearerTokenAuth::new("correct-token".into());

        let mut headers = hyper::HeaderMap::new();
        headers.insert("authorization", "Bearer wrong-token".parse().unwrap());

        assert!(auth.validate_request(&headers).await.is_err());
    }

    #[tokio::test]
    async fn validate_rejects_missing_header() {
        let auth = BearerTokenAuth::new("some-token".into());
        let headers = hyper::HeaderMap::new();
        assert!(auth.validate_request(&headers).await.is_err());
    }

    #[tokio::test]
    async fn validate_rejects_non_bearer_scheme() {
        let auth = BearerTokenAuth::new("some-token".into());

        let mut headers = hyper::HeaderMap::new();
        headers.insert("authorization", "Basic dXNlcjpwYXNz".parse().unwrap());

        assert!(auth.validate_request(&headers).await.is_err());
    }

    #[tokio::test]
    async fn apply_adds_bearer_header() {
        let auth = BearerTokenAuth::new("my-token".into());
        let client = reqwest::Client::new();
        let req = client.post("http://localhost/test");
        let req = auth.apply_to_request(req).await.unwrap();

        let built = req.build().unwrap();
        let header = built
            .headers()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(header, "Bearer my-token");
    }
}
