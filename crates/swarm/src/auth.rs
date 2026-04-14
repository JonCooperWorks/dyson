//! Bearer-token issuance and extraction.
//!
//! Authentication model for v1:
//!
//! - On `POST /swarm/register`, the hub generates a cryptographically
//!   random 32-byte token, base64-encodes it, and returns it alongside
//!   the assigned node_id.  The registry stores the token.
//! - Every authed request sends `Authorization: Bearer <token>`.
//! - On the server side, handlers take an `AuthedNode` extractor which
//!   looks up the token in the registry and yields the `NodeId`.
//!
//! That's the whole threat model.  No JWTs, no OAuth, no rotation.

use std::sync::Arc;

use axum::extract::{FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ring::rand::{SecureRandom, SystemRandom};

use crate::Hub;

/// Length of the random bytes that back a bearer token.
const TOKEN_BYTES: usize = 32;

/// Generate a fresh, cryptographically random bearer token.
pub fn generate_token() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    SystemRandom::new()
        .fill(&mut buf)
        .expect("system RNG failed");
    STANDARD.encode(buf)
}

/// Parse a bearer token out of the `Authorization` header.
///
/// Returns `None` if the header is absent, malformed, or doesn't start
/// with `Bearer `.
pub fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<String> {
    let val = headers.get(axum::http::header::AUTHORIZATION)?;
    let text = val.to_str().ok()?;
    let token = text.strip_prefix("Bearer ").or_else(|| text.strip_prefix("bearer "))?;
    Some(token.trim().to_string())
}

/// An authenticated node extracted from the request.
///
/// Use this as a handler parameter to require auth:
///
/// ```ignore
/// async fn handler(
///     State(hub): State<Arc<Hub>>,
///     AuthedNode(node_id): AuthedNode,
/// ) -> impl IntoResponse { ... }
/// ```
pub struct AuthedNode(pub String);

#[axum::async_trait]
impl FromRequestParts<Arc<Hub>> for AuthedNode {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<Hub>,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_bearer(&parts.headers)
            .ok_or((StatusCode::UNAUTHORIZED, "missing or malformed Authorization header"))?;

        let hub = State::<Arc<Hub>>(Arc::clone(state));
        let node_id = hub
            .0
            .registry
            .node_id_for_token(&token)
            .await
            .ok_or((StatusCode::UNAUTHORIZED, "unknown bearer token"))?;

        Ok(Self(node_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use axum::http::header::AUTHORIZATION;

    #[test]
    fn generate_token_is_non_empty_and_unique() {
        let a = generate_token();
        let b = generate_token();
        assert!(!a.is_empty());
        assert!(!b.is_empty());
        assert_ne!(a, b);
    }

    #[test]
    fn extract_bearer_ok() {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, "Bearer abc123".parse().unwrap());
        assert_eq!(extract_bearer(&h).as_deref(), Some("abc123"));
    }

    #[test]
    fn extract_bearer_case_insensitive() {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, "bearer xyz".parse().unwrap());
        assert_eq!(extract_bearer(&h).as_deref(), Some("xyz"));
    }

    #[test]
    fn extract_bearer_missing() {
        let h = HeaderMap::new();
        assert!(extract_bearer(&h).is_none());
    }

    #[test]
    fn extract_bearer_wrong_scheme() {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, "Basic dXNlcjpwYXNz".parse().unwrap());
        assert!(extract_bearer(&h).is_none());
    }
}
