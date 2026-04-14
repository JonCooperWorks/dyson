// ===========================================================================
// DeferredBearerAuth — Bearer auth whose token arrives after construction.
//
// Used by the swarm MCP skill: the skill is injected into agent settings
// before the SwarmController registers with the hub, so the bearer token
// is not yet available.  After registration the controller writes the
// token into a `watch::Sender`; this auth reads the latest value on
// each request.
//
// Before the token is set, `apply_to_request` is a no-op (no auth
// header).  Once set, it adds `Authorization: Bearer <token>`.
// ===========================================================================

use async_trait::async_trait;
use tokio::sync::watch;

use crate::auth::Auth;
use crate::error::Result;

/// Bearer auth that reads its token from a `watch` channel.
///
/// The token can be updated at any time (e.g. after reconnection with
/// a new registration token).  Each outgoing request reads the latest
/// value.
pub struct DeferredBearerAuth {
    rx: watch::Receiver<Option<String>>,
}

impl DeferredBearerAuth {
    /// Create from the receiving end of a watch channel.
    ///
    /// The sending end should be held by the component that obtains
    /// the token (e.g. `SwarmController` after registration).
    pub const fn new(rx: watch::Receiver<Option<String>>) -> Self {
        Self { rx }
    }
}

#[async_trait]
impl Auth for DeferredBearerAuth {
    async fn apply_to_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder> {
        let token = self.rx.borrow().clone();
        match token {
            Some(t) => Ok(request.bearer_auth(t)),
            None => Ok(request),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_token_is_noop() {
        let (_tx, rx) = watch::channel(None);
        let auth = DeferredBearerAuth::new(rx);
        let headers = crate::auth::test_apply(&auth).await;
        assert!(!headers.contains_key("authorization"));
    }

    #[tokio::test]
    async fn token_adds_bearer_header() {
        let (tx, rx) = watch::channel(None);
        let auth = DeferredBearerAuth::new(rx);

        tx.send(Some("my-secret-token".into())).unwrap();

        let headers = crate::auth::test_apply(&auth).await;
        let val = headers.get("authorization").unwrap().to_str().unwrap();
        assert_eq!(val, "Bearer my-secret-token");
    }

    #[tokio::test]
    async fn token_update_reflected_on_next_request() {
        let (tx, rx) = watch::channel(Some("token-v1".into()));
        let auth = DeferredBearerAuth::new(rx);

        let h1 = crate::auth::test_apply(&auth).await;
        assert!(h1.get("authorization").unwrap().to_str().unwrap().contains("token-v1"));

        tx.send(Some("token-v2".into())).unwrap();

        let h2 = crate::auth::test_apply(&auth).await;
        assert!(h2.get("authorization").unwrap().to_str().unwrap().contains("token-v2"));
    }
}
