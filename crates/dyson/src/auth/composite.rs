// ===========================================================================
// CompositeAuth — chain multiple Auth implementations in sequence.
//
// Each layer's `apply_to_request` is called in order, passing the builder
// through the chain.  For `validate_request`, the first layer that succeeds
// wins (short-circuit on success, try next on failure).
// ===========================================================================

use async_trait::async_trait;

use crate::auth::{Auth, AuthInfo};
use crate::error::{DysonError, Result};

/// Chains multiple `Auth` implementations.
///
/// `apply_to_request`: all layers run in order (each adds its headers).
/// `validate_request`: first successful layer wins.
pub struct CompositeAuth {
    layers: Vec<Box<dyn Auth>>,
}

impl CompositeAuth {
    pub fn new(layers: Vec<Box<dyn Auth>>) -> Self {
        Self { layers }
    }
}

#[async_trait]
impl Auth for CompositeAuth {
    async fn apply_to_request(
        &self,
        mut request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder> {
        for layer in &self.layers {
            request = layer.apply_to_request(request).await?;
        }
        Ok(request)
    }

    async fn validate_request(&self, headers: &hyper::HeaderMap) -> Result<AuthInfo> {
        for layer in &self.layers {
            if let Ok(info) = layer.validate_request(headers).await {
                return Ok(info);
            }
        }
        Err(DysonError::Config(
            "no auth layer accepted the request".into(),
        ))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{ApiKeyAuth, BearerTokenAuth, DangerousNoAuth};

    #[tokio::test]
    async fn apply_chains_all_layers() {
        let composite = CompositeAuth::new(vec![
            Box::new(BearerTokenAuth::new("my-token".into())),
            Box::new(ApiKeyAuth::new("x-custom", "val".into())),
        ]);

        let headers = super::super::test_apply(&composite).await;
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer my-token"
        );
        assert_eq!(headers.get("x-custom").unwrap().to_str().unwrap(), "val");
    }

    #[tokio::test]
    async fn validate_first_success_wins() {
        let composite = CompositeAuth::new(vec![
            Box::new(BearerTokenAuth::new("token-a".into())),
            Box::new(DangerousNoAuth),
        ]);

        // Bearer check fails (no header), but DangerousNoAuth succeeds.
        let headers = hyper::HeaderMap::new();
        let info = composite.validate_request(&headers).await.unwrap();
        assert_eq!(info.identity, "anonymous");
    }

    #[tokio::test]
    async fn validate_all_fail() {
        let composite = CompositeAuth::new(vec![
            Box::new(BearerTokenAuth::new("a".into())),
            Box::new(BearerTokenAuth::new("b".into())),
        ]);

        let headers = hyper::HeaderMap::new();
        assert!(composite.validate_request(&headers).await.is_err());
    }

    #[tokio::test]
    async fn empty_composite_apply_is_noop() {
        let composite = CompositeAuth::new(vec![]);
        let headers = super::super::test_apply(&composite).await;
        assert!(!headers.contains_key("authorization"));
    }
}
