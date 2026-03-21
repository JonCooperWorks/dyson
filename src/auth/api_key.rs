// ===========================================================================
// ApiKeyAuth — send a named header with an API key value.
//
// Used by:
//   - Anthropic client: `x-api-key: <key>`
//   - Any provider that uses a non-Bearer API key header
//
// Memory safety:
//   Uses `Credential` internally — the key is zeroized on drop.
// ===========================================================================

use async_trait::async_trait;

use crate::auth::{Auth, AuthInfo, Credential};
use crate::error::Result;

/// API key authentication via a custom header.
///
/// Sends `<header_name>: <key>` with every outgoing request.
/// Also supports server-side validation of the same header.
pub struct ApiKeyAuth {
    header_name: String,
    key: Credential,
}

impl ApiKeyAuth {
    /// Create with an arbitrary header name and key value.
    pub fn new(header_name: impl Into<String>, key: String) -> Self {
        Self {
            header_name: header_name.into(),
            key: Credential::new(key),
        }
    }

    /// Convenience: Anthropic-style `x-api-key` header.
    pub fn anthropic(key: String) -> Self {
        Self::new("x-api-key", key)
    }
}

#[async_trait]
impl Auth for ApiKeyAuth {
    async fn apply_to_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder> {
        Ok(request.header(&self.header_name, self.key.expose()))
    }

    async fn validate_request(&self, headers: &hyper::HeaderMap) -> Result<AuthInfo> {
        let valid = headers
            .get(&self.header_name)
            .and_then(|v| v.to_str().ok())
            .map(|v| v == self.key.expose())
            .unwrap_or(false);

        if valid {
            Ok(AuthInfo::new(format!("api-key:{}", self.header_name)))
        } else {
            Err(crate::error::DysonError::Config("unauthorized".into()))
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn apply_adds_custom_header() {
        let auth = ApiKeyAuth::new("x-api-key", "sk-ant-test".into());
        let client = reqwest::Client::new();
        let req = client.post("http://localhost/test");
        let req = auth.apply_to_request(req).await.unwrap();

        let built = req.build().unwrap();
        let header = built.headers().get("x-api-key").unwrap().to_str().unwrap();
        assert_eq!(header, "sk-ant-test");
    }

    #[tokio::test]
    async fn anthropic_factory() {
        let auth = ApiKeyAuth::anthropic("sk-ant-key".into());
        let client = reqwest::Client::new();
        let req = client.post("http://localhost/test");
        let req = auth.apply_to_request(req).await.unwrap();

        let built = req.build().unwrap();
        assert!(built.headers().contains_key("x-api-key"));
    }

    #[tokio::test]
    async fn validate_accepts_matching_key() {
        let auth = ApiKeyAuth::new("x-api-key", "my-key".into());

        let mut headers = hyper::HeaderMap::new();
        headers.insert("x-api-key", "my-key".parse().unwrap());

        let info = auth.validate_request(&headers).await.unwrap();
        assert_eq!(info.identity, "api-key:x-api-key");
    }

    #[tokio::test]
    async fn validate_rejects_wrong_key() {
        let auth = ApiKeyAuth::new("x-api-key", "correct-key".into());

        let mut headers = hyper::HeaderMap::new();
        headers.insert("x-api-key", "wrong-key".parse().unwrap());

        assert!(auth.validate_request(&headers).await.is_err());
    }
}
