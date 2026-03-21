// ===========================================================================
// TracingAuth — audit wrapper that logs all auth events via tracing.
//
// Wraps any `Auth` implementation and emits structured tracing events for
// every `apply_to_request` and `validate_request` call.  Users who want
// custom audit logging can subscribe to tracing events with a custom
// subscriber — no need for callback functions.
//
// This follows the existing pattern in the codebase: serve.rs already uses
// `tracing::warn!` for rejected auth.  TracingAuth generalizes this.
// ===========================================================================

use async_trait::async_trait;

use crate::auth::{Auth, AuthInfo};
use crate::error::Result;

/// Audit wrapper — delegates to an inner `Auth` and logs all auth events.
///
/// ```ignore
/// let auth = TracingAuth::new(
///     Box::new(BearerTokenAuth::new(token)),
///     "mcp-server",
/// );
/// ```
///
/// Emits tracing events at these levels:
/// - `info`: successful auth (validate OK, apply OK)
/// - `warn`: failed auth (validate rejected)
/// - `debug`: apply_to_request calls (high volume)
pub struct TracingAuth {
    inner: Box<dyn Auth>,
    label: String,
}

impl TracingAuth {
    pub fn new(inner: Box<dyn Auth>, label: impl Into<String>) -> Self {
        Self {
            inner,
            label: label.into(),
        }
    }
}

#[async_trait]
impl Auth for TracingAuth {
    async fn apply_to_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder> {
        let result = self.inner.apply_to_request(request).await;
        match &result {
            Ok(_) => {
                tracing::debug!(label = %self.label, "auth: credentials applied to outgoing request");
            }
            Err(e) => {
                tracing::warn!(label = %self.label, error = %e, "auth: failed to apply credentials");
            }
        }
        result
    }

    async fn validate_request(&self, headers: &hyper::HeaderMap) -> Result<AuthInfo> {
        match self.inner.validate_request(headers).await {
            Ok(info) => {
                tracing::info!(
                    label = %self.label,
                    identity = %info.identity,
                    "auth: request authenticated"
                );
                Ok(info)
            }
            Err(e) => {
                tracing::warn!(
                    label = %self.label,
                    error = %e,
                    "auth: request rejected"
                );
                Err(e)
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{BearerTokenAuth, NoAuth};

    #[tokio::test]
    async fn delegates_apply() {
        let auth = TracingAuth::new(
            Box::new(BearerTokenAuth::new("test-token".into())),
            "test",
        );

        let client = reqwest::Client::new();
        let req = client.post("http://localhost/test");
        let req = auth.apply_to_request(req).await.unwrap();

        let built = req.build().unwrap();
        assert_eq!(
            built.headers().get("authorization").unwrap().to_str().unwrap(),
            "Bearer test-token"
        );
    }

    #[tokio::test]
    async fn delegates_validate_success() {
        let auth = TracingAuth::new(Box::new(NoAuth), "test");
        let headers = hyper::HeaderMap::new();
        let info = auth.validate_request(&headers).await.unwrap();
        assert_eq!(info.identity, "anonymous");
    }

    #[tokio::test]
    async fn delegates_validate_failure() {
        let auth = TracingAuth::new(
            Box::new(BearerTokenAuth::new("correct".into())),
            "test",
        );

        let mut headers = hyper::HeaderMap::new();
        headers.insert("authorization", "Bearer wrong".parse().unwrap());

        assert!(auth.validate_request(&headers).await.is_err());
    }
}
