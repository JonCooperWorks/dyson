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
    pub fn new(headers: HashMap<String, String>) -> Self {
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
        let mut headers = HashMap::new();
        headers.insert("X-Api-Key".into(), "key-123".into());
        headers.insert("X-Custom".into(), "custom-val".into());

        let auth = StaticHeadersAuth::new(headers);
        let client = reqwest::Client::new();
        let req = client.post("http://localhost/test");
        let req = auth.apply_to_request(req).await.unwrap();

        let built = req.build().unwrap();
        assert_eq!(
            built.headers().get("x-api-key").unwrap().to_str().unwrap(),
            "key-123"
        );
        assert_eq!(
            built.headers().get("x-custom").unwrap().to_str().unwrap(),
            "custom-val"
        );
    }

    #[tokio::test]
    async fn empty_headers_is_noop() {
        let auth = StaticHeadersAuth::new(HashMap::new());
        let client = reqwest::Client::new();
        let req = client.post("http://localhost/test");
        let req = auth.apply_to_request(req).await.unwrap();

        let built = req.build().unwrap();
        // Only default headers (if any), no custom ones.
        assert!(!built.headers().contains_key("x-api-key"));
    }
}
