// ===========================================================================
// HashedBearerAuth — server-side Argon2id-verified bearer tokens.
//
// `BearerTokenAuth` keeps a plaintext token in memory because it has to —
// the MCP server mints a per-session token, hands it to a Claude Code
// subprocess, and validates the same string on each callback.  The HTTP
// controller's config is the opposite shape: a long-lived secret an
// operator pastes into a Tailscale browser, which we MUST NOT keep on
// disk in clear text.  This auth stores the Argon2id PHC hash and runs
// `verify_password` on each `/api/*` request.  Server-side only —
// `apply_to_request` falls through to the trait default (no-op pass-
// through).  We never have the plaintext, so we couldn't apply it to an
// outbound request even if we wanted to.
// ===========================================================================

use argon2::password_hash::PasswordHash;
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use async_trait::async_trait;

use crate::auth::{Auth, AuthInfo};
use crate::error::{DysonError, Result};

/// Bearer-token auth that compares against a stored Argon2id hash.
///
/// Holds only the PHC hash — no plaintext.  Construct via `from_phc`
/// (validates format up front) and validate incoming requests with the
/// `Auth::validate_request` impl.
pub struct HashedBearerAuth {
    /// Full Argon2 PHC string, e.g. `$argon2id$v=19$m=19456,t=2,p=1$...$...`.
    /// Kept as an owned `String` because `PasswordHash<'a>` borrows from
    /// the source slice; constructing one per request is cheap relative
    /// to the verify itself.
    phc: String,
}

impl HashedBearerAuth {
    /// Build from an Argon2 PHC string.  Returns an error if the string
    /// isn't a valid PHC encoding so a typo'd config fails at startup
    /// rather than silently rejecting every request later.
    pub fn from_phc(phc: String) -> Result<Self> {
        PasswordHash::new(&phc)
            .map_err(|e| DysonError::Config(format!("invalid argon2 hash: {e}")))?;
        Ok(Self { phc })
    }

    /// Hash a plaintext token with Argon2id default params and return
    /// the PHC string.  Used by the `dyson hash-bearer` CLI subcommand
    /// so operators can paste the result straight into dyson.json.
    ///
    /// Salt comes from the workspace `rand` crate's CSPRNG (a thin shim
    /// over OS getrandom) rather than `password_hash::rand_core::OsRng`
    /// — the latter is gated behind a feature that conflicts with our
    /// `rand 0.10` upgrade, and dyson already trusts `rand::rng()`
    /// elsewhere (see `bearer::generate`).
    pub fn hash(plaintext: &str) -> Result<String> {
        use argon2::password_hash::SaltString;
        use rand::RngExt;

        // 16 bytes is argon2's recommended salt minimum and what
        // `SaltString::generate` would have produced.
        let mut salt_bytes = [0u8; 16];
        rand::rng().fill(&mut salt_bytes);
        let salt = SaltString::encode_b64(&salt_bytes)
            .map_err(|e| DysonError::Config(format!("salt encode failed: {e}")))?;
        Argon2::default()
            .hash_password(plaintext.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| DysonError::Config(format!("argon2 hash failed: {e}")))
    }
}

#[async_trait]
impl Auth for HashedBearerAuth {
    async fn validate_request(&self, headers: &hyper::HeaderMap) -> Result<AuthInfo> {
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| DysonError::Config("unauthorized".into()))?;

        // Re-parse the PHC every time.  Argon2id's verify dwarfs the
        // parse cost, and `PasswordHash` borrows from `self.phc` so we
        // can't cache a parsed value across awaits without self-
        // referential gymnastics.
        let parsed = PasswordHash::new(&self.phc)
            .map_err(|_| DysonError::Config("unauthorized".into()))?;

        Argon2::default()
            .verify_password(token.as_bytes(), &parsed)
            .map(|_| AuthInfo::new("bearer"))
            .map_err(|_| DysonError::Config("unauthorized".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn validates_matching_token() {
        let phc = HashedBearerAuth::hash("super-secret").unwrap();
        let auth = HashedBearerAuth::from_phc(phc).unwrap();

        let mut headers = hyper::HeaderMap::new();
        headers.insert("authorization", "Bearer super-secret".parse().unwrap());

        let info = auth.validate_request(&headers).await.unwrap();
        assert_eq!(info.identity, "bearer");
    }

    #[tokio::test]
    async fn rejects_wrong_token() {
        let phc = HashedBearerAuth::hash("right").unwrap();
        let auth = HashedBearerAuth::from_phc(phc).unwrap();

        let mut headers = hyper::HeaderMap::new();
        headers.insert("authorization", "Bearer wrong".parse().unwrap());

        assert!(auth.validate_request(&headers).await.is_err());
    }

    #[tokio::test]
    async fn rejects_missing_header() {
        let phc = HashedBearerAuth::hash("any").unwrap();
        let auth = HashedBearerAuth::from_phc(phc).unwrap();
        assert!(auth.validate_request(&hyper::HeaderMap::new()).await.is_err());
    }

    #[test]
    fn from_phc_rejects_bad_input() {
        assert!(HashedBearerAuth::from_phc("not-a-phc".to_string()).is_err());
    }

    #[test]
    fn hash_uses_argon2id_phc_format() {
        let phc = HashedBearerAuth::hash("x").unwrap();
        assert!(phc.starts_with("$argon2id$"), "got: {phc}");
    }
}
