// ===========================================================================
// HashedBearerAuth — server-side Argon2id-verified bearer tokens.
//
// `BearerTokenAuth` keeps a plaintext token in memory because it has to —
// the MCP server mints a per-session token, hands it to a Claude Code
// subprocess, and validates the same string on each callback.  The HTTP
// controller's config is the opposite shape: a long-lived secret an
// operator pastes into a Tailscale browser, which we MUST NOT keep on
// disk in clear text.  This auth stores the Argon2id PHC hash and runs
// `verify_password` on a blocking thread, at most a few at a time, then
// trusts that exact token for a short window.  Server-side only —
// `apply_to_request` falls through to the trait default (no-op pass-
// through).  We never have the plaintext, so we couldn't apply it to an
// outbound request even if we wanted to.
// ===========================================================================

use std::sync::Arc;
use std::time::{Duration, Instant};

use argon2::password_hash::PasswordHash;
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;

use crate::auth::{Auth, AuthInfo};
use dyson_core::error::{DysonError, Result};

/// How long a verified token is trusted without re-running Argon2.  The
/// SPA and Swarm's proxy send the same token on every request, so even a
/// short window turns a verify per request into a verify per half
/// minute.  Kept short so a rotated hash (restart) or a revoked browser
/// session stops working promptly.
const VERIFIED_TOKEN_TTL: Duration = Duration::from_secs(30);

/// Argon2 verifies allowed in flight at once.  Each one is ~19 MiB and a
/// blocking-pool thread for tens of milliseconds; without a bound, a
/// flood of wrong tokens (which can never hit the cache) would fill the
/// blocking pool that file I/O and other `spawn_blocking` work share.
const MAX_CONCURRENT_VERIFIES: usize = 4;

/// Bearer-token auth that compares against a stored Argon2id hash.
///
/// Holds only the PHC hash — no plaintext.  Construct via `from_phc`
/// (validates format up front) and validate incoming requests with the
/// `Auth::validate_request` impl.
pub struct HashedBearerAuth {
    /// Full Argon2 PHC string, e.g. `$argon2id$v=19$m=19456,t=2,p=1$...$...`.
    /// `Arc<str>` so each verify can move it onto a blocking thread
    /// without copying; `PasswordHash<'a>` borrows from it there.
    phc: Arc<str>,
    /// Per-process key for `token_digest`, so the cached digest is not a
    /// plain SHA-256 of the token that a memory dump could brute-force
    /// offline faster than the Argon2 hash.
    digest_key: [u8; 32],
    /// Digest of the last token Argon2 accepted, and when that stops
    /// counting.  Only ever written after a successful verify.
    verified: std::sync::Mutex<Option<([u8; 32], Instant)>>,
    verify_slots: Arc<Semaphore>,
    #[cfg(test)]
    probe: Arc<test_probe::Probe>,
}

impl HashedBearerAuth {
    /// Build from an Argon2 PHC string.  Returns an error if the string
    /// isn't a valid PHC encoding so a typo'd config fails at startup
    /// rather than silently rejecting every request later.
    pub fn from_phc(phc: String) -> Result<Self> {
        use rand::RngExt;

        PasswordHash::new(&phc)
            .map_err(|e| DysonError::Config(format!("invalid argon2 hash: {e}")))?;
        let mut digest_key = [0u8; 32];
        rand::rng().fill(&mut digest_key);
        Ok(Self {
            phc: Arc::from(phc),
            digest_key,
            verified: std::sync::Mutex::new(None),
            verify_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_VERIFIES)),
            #[cfg(test)]
            probe: Arc::default(),
        })
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

    fn token_digest(&self, token: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.digest_key);
        hasher.update(token.as_bytes());
        hasher.finalize().into()
    }

    fn cache_hit(&self, digest: &[u8; 32]) -> bool {
        let guard = match self.verified.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match guard.as_ref() {
            Some((cached, expires_at)) => {
                *expires_at > Instant::now() && bool::from(cached.ct_eq(digest))
            }
            None => false,
        }
    }

    fn remember(&self, digest: [u8; 32]) {
        let mut guard = match self.verified.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *guard = Some((digest, Instant::now() + VERIFIED_TOKEN_TTL));
    }
}

#[async_trait]
impl Auth for HashedBearerAuth {
    async fn validate_request(&self, headers: &hyper::HeaderMap) -> Result<AuthInfo> {
        let token = super::extract_bearer(headers)
            .ok_or_else(|| DysonError::Config("unauthorized".into()))?;

        let digest = self.token_digest(token);
        if self.cache_hit(&digest) {
            return Ok(AuthInfo::new("bearer"));
        }

        // The permit moves into the blocking closure, so it is held for
        // as long as Argon2 actually runs even if this request is dropped
        // (client gone) while waiting on the join handle.
        let permit = Arc::clone(&self.verify_slots)
            .acquire_owned()
            .await
            .map_err(|_| DysonError::Config("unauthorized".into()))?;
        let phc = Arc::clone(&self.phc);
        let token = token.to_owned();
        #[cfg(test)]
        let probe = Arc::clone(&self.probe);
        let verified = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            #[cfg(test)]
            let _running = probe.enter();
            PasswordHash::new(&phc).is_ok_and(|parsed| {
                Argon2::default()
                    .verify_password(token.as_bytes(), &parsed)
                    .is_ok()
            })
        })
        .await
        .unwrap_or(false);

        if verified {
            self.remember(digest);
            Ok(AuthInfo::new("bearer"))
        } else {
            Err(DysonError::Config("unauthorized".into()))
        }
    }
}

#[cfg(test)]
mod test_probe {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts Argon2 runs and the most that were ever in flight at once.
    #[derive(Default)]
    pub(super) struct Probe {
        pub(super) runs: AtomicUsize,
        running: AtomicUsize,
        pub(super) peak: AtomicUsize,
    }

    pub(super) struct Running<'a>(&'a Probe);

    impl Probe {
        pub(super) fn enter(&self) -> Running<'_> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            Running(self)
        }
    }

    impl Drop for Running<'_> {
        fn drop(&mut self) {
            self.0.running.fetch_sub(1, Ordering::SeqCst);
        }
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
        assert!(
            auth.validate_request(&hyper::HeaderMap::new())
                .await
                .is_err()
        );
    }

    fn headers_for(token: &str) -> hyper::HeaderMap {
        let mut h = hyper::HeaderMap::new();
        h.insert("authorization", format!("Bearer {token}").parse().unwrap());
        h
    }

    #[tokio::test]
    async fn verified_token_is_cached_and_failures_are_not() {
        use std::sync::atomic::Ordering;
        let auth = HashedBearerAuth::from_phc(HashedBearerAuth::hash("right").unwrap()).unwrap();

        // A wrong token is verified (and refused) every time: a failure
        // must never become a cache entry.
        assert!(auth.validate_request(&headers_for("wrong")).await.is_err());
        assert!(auth.validate_request(&headers_for("wrong")).await.is_err());
        assert_eq!(auth.probe.runs.load(Ordering::SeqCst), 2);

        // The right token pays Argon2 once, then rides the cache.
        for _ in 0..5 {
            auth.validate_request(&headers_for("right")).await.unwrap();
        }
        assert_eq!(auth.probe.runs.load(Ordering::SeqCst), 3);

        // A cached good token does not make a different one pass.
        assert!(auth.validate_request(&headers_for("righ")).await.is_err());
        assert!(auth.validate_request(&headers_for("right ")).await.is_ok());
        assert_eq!(auth.probe.runs.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn cached_token_expires() {
        use std::sync::atomic::Ordering;
        let auth = HashedBearerAuth::from_phc(HashedBearerAuth::hash("t").unwrap()).unwrap();
        auth.validate_request(&headers_for("t")).await.unwrap();
        // Age the entry past its TTL instead of sleeping for it.
        if let Some((_, expires_at)) = auth.verified.lock().unwrap().as_mut() {
            *expires_at = Instant::now() - Duration::from_millis(1);
        }
        auth.validate_request(&headers_for("t")).await.unwrap();
        assert_eq!(auth.probe.runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bad_token_flood_runs_a_bounded_number_of_verifies() {
        use std::sync::atomic::Ordering;
        let auth =
            Arc::new(HashedBearerAuth::from_phc(HashedBearerAuth::hash("right").unwrap()).unwrap());
        let mut tasks = Vec::new();
        for i in 0..24 {
            let auth = Arc::clone(&auth);
            tasks.push(tokio::spawn(async move {
                auth.validate_request(&headers_for(&format!("wrong-{i}")))
                    .await
                    .is_err()
            }));
        }
        for t in tasks {
            assert!(t.await.unwrap());
        }
        assert_eq!(auth.probe.runs.load(Ordering::SeqCst), 24);
        let peak = auth.probe.peak.load(Ordering::SeqCst);
        assert!(
            (1..=MAX_CONCURRENT_VERIFIES).contains(&peak),
            "{peak} verifies ran at once"
        );
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

    // -----------------------------------------------------------------
    // Boundary cases: a plaintext token can be just about anything an
    // operator pastes into a config or a browser, so the hash↔verify
    // pipeline has to survive the unusual shapes a real user produces.
    // -----------------------------------------------------------------

    #[test]
    fn from_phc_rejects_malformed_strings_with_distinct_errors() {
        // Empty string — nothing to parse.
        assert!(HashedBearerAuth::from_phc(String::new()).is_err());
        // Missing the `$` prefix entirely.
        assert!(HashedBearerAuth::from_phc("argon2id$v=19$...".into()).is_err());
        // Truncated — algorithm tag with no params.
        assert!(HashedBearerAuth::from_phc("$argon2id$".into()).is_err());
        // Bytes mid-string.
        assert!(HashedBearerAuth::from_phc("$argon2id$\0".into()).is_err());
        // Wrong algorithm tag.
        assert!(HashedBearerAuth::from_phc("$bcrypt$v=2$cost=12$...$...".into()).is_err());
    }

    #[tokio::test]
    async fn tolerates_surrounding_whitespace_around_the_token() {
        // The shared `strip_bearer` trims surrounding whitespace: a
        // `Bearer  right` (two spaces) or `Bearer right ` (trailing space)
        // both yield the token "right".  That's lenient-but-safe — the
        // trimmed token still has to match the stored hash, so padding
        // can't smuggle a different credential past verification.
        let phc = HashedBearerAuth::hash("right").unwrap();
        let auth = HashedBearerAuth::from_phc(phc).unwrap();
        let mut h = hyper::HeaderMap::new();
        h.insert("authorization", "Bearer  right".parse().unwrap());
        assert!(auth.validate_request(&h).await.is_ok());
        h.clear();
        h.insert("authorization", "Bearer right ".parse().unwrap());
        assert!(auth.validate_request(&h).await.is_ok());
    }

    #[tokio::test]
    async fn accepts_lowercase_bearer_prefix() {
        // RFC 7235 makes the auth scheme case-insensitive, so the shared
        // `strip_bearer` accepts a downcased `bearer ` scheme.  A client
        // that lowercases the scheme verifies normally against the stored
        // hash; the token itself is still matched byte-for-byte.
        let phc = HashedBearerAuth::hash("token").unwrap();
        let auth = HashedBearerAuth::from_phc(phc).unwrap();
        let mut h = hyper::HeaderMap::new();
        h.insert("authorization", "bearer token".parse().unwrap());
        assert!(auth.validate_request(&h).await.is_ok());
    }

    #[tokio::test]
    async fn rejects_non_ascii_authorization_header_value() {
        // HTTP header values must be visible ASCII; an operator who
        // pastes an emoji-laced password into the browser would get a
        // header that hyper accepts on the wire (any byte is valid)
        // but our `to_str()` rejects.  That's the correct behaviour —
        // pin it so a future "to_str_lossy" doesn't silently let
        // unicode bytes leak past the prefix-strip and produce a
        // mismatched verify call.
        let phc = HashedBearerAuth::hash("token").unwrap();
        let auth = HashedBearerAuth::from_phc(phc).unwrap();
        let mut h = hyper::HeaderMap::new();
        // 0xC3 0xA9 is UTF-8 'é' — invalid in an RFC 7230 header value.
        let bytes = b"Bearer caf\xC3\xA9";
        h.insert(
            "authorization",
            hyper::header::HeaderValue::from_bytes(bytes).unwrap(),
        );
        assert!(auth.validate_request(&h).await.is_err());
    }

    #[tokio::test]
    async fn validates_punctuation_heavy_token() {
        // Real-world bearer tokens often look like base64url with `-_=`
        // or have URL-safe ASCII punctuation.  Make sure the strip-
        // prefix + verify chain doesn't depend on the alphabet.
        let token = "abcXYZ-_.~!@#$%^&*()=+,;:?[]{}";
        let phc = HashedBearerAuth::hash(token).unwrap();
        let auth = HashedBearerAuth::from_phc(phc).unwrap();
        let mut h = hyper::HeaderMap::new();
        h.insert("authorization", format!("Bearer {token}").parse().unwrap());
        assert!(auth.validate_request(&h).await.is_ok());
    }

    #[tokio::test]
    async fn validates_long_token() {
        // A 4 KiB pasted token is silly but legal — argon2 hashes the
        // raw bytes regardless of length.  Make sure we don't hit any
        // hidden cap.
        let token: String = "long-".repeat(800);
        let phc = HashedBearerAuth::hash(&token).unwrap();
        let auth = HashedBearerAuth::from_phc(phc).unwrap();
        let mut h = hyper::HeaderMap::new();
        h.insert("authorization", format!("Bearer {token}").parse().unwrap());
        assert!(auth.validate_request(&h).await.is_ok());
    }

    #[tokio::test]
    async fn rejects_when_phc_params_mismatch_what_hashed() {
        // Hand-craft a PHC string whose parameters are valid but whose
        // hash bytes are wrong — typo a single character of the encoded
        // hash and the verifier must say no.  This guards against any
        // future "constant-time only on the right path" regression.
        let original = HashedBearerAuth::hash("token").unwrap();
        // PHC is `$...$<salt>$<hash>`. Flip the first character of the
        // hash segment, not the tail: the final base64 character may
        // encode unused bits, and changing it can make the PHC invalid
        // before verification reaches the intended bytes-mismatch path.
        let (prefix, hash) = original.rsplit_once('$').expect("hash segment");
        let mut chars: Vec<char> = hash.chars().collect();
        let first = chars.first_mut().expect("non-empty hash segment");
        *first = if *first == 'A' { 'B' } else { 'A' };
        let tampered_hash: String = chars.into_iter().collect();
        let tampered = format!("{prefix}${tampered_hash}");
        let auth = HashedBearerAuth::from_phc(tampered).unwrap();
        let mut h = hyper::HeaderMap::new();
        h.insert("authorization", "Bearer token".parse().unwrap());
        assert!(
            auth.validate_request(&h).await.is_err(),
            "tampered hash must reject"
        );
    }
}
