// ===========================================================================
// StaticHeadersAuth — apply a fixed set of headers to outgoing requests.
//
// Used by:
//   - HttpTransport for MCP client connections where the config specifies
//     arbitrary headers (API keys, custom auth tokens, etc.)
//
// This replaces the raw `HashMap<String, String>` that HttpTransport
// previously managed directly.
// ===========================================================================

use std::collections::HashMap;

use async_trait::async_trait;

use crate::auth::Auth;
use crate::error::Result;

/// Applies a static set of headers to every outgoing request.
///
/// Constructed from the `headers` map in MCP HTTP transport config:
///
/// ```json
/// {
///   "url": "https://mcp.example.com/mcp",
///   "headers": {
///     "Authorization": "Bearer sk-...",
///     "X-Custom": "value"
///   }
/// }
/// ```
pub struct StaticHeadersAuth {
    headers: HashMap<String, String>,
}

impl StaticHeadersAuth {
    pub const fn new(headers: HashMap<String, String>) -> Self {
        Self { headers }
    }
}

#[async_trait]
impl Auth for StaticHeadersAuth {
    async fn apply_to_request(
        &self,
        mut request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder> {
        for (key, value) in &self.headers {
            request = request.header(key.as_str(), value.as_str());
        }
        Ok(request)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn applies_all_headers() {
        let mut h = HashMap::new();
        h.insert("X-Api-Key".into(), "key-123".into());
        h.insert("X-Custom".into(), "custom-val".into());

        let auth = StaticHeadersAuth::new(h);
        let headers = super::super::test_apply(&auth).await;
        assert_eq!(
            headers.get("x-api-key").unwrap().to_str().unwrap(),
            "key-123"
        );
        assert_eq!(
            headers.get("x-custom").unwrap().to_str().unwrap(),
            "custom-val"
        );
    }

    #[tokio::test]
    async fn empty_headers_is_noop() {
        let auth = StaticHeadersAuth::new(HashMap::new());
        let headers = super::super::test_apply(&auth).await;
        // Only default headers (if any), no custom ones.
        assert!(!headers.contains_key("x-api-key"));
    }
}
