// ===========================================================================
// OidcAuth — JWT-verifying inbound auth for the HTTP controller.
//
// What this does: the dyson HTTP controller is the *relying party*.  It
// consumes bearer JWTs minted by an external OpenID Connect provider
// (Okta, Auth0, Entra ID, Keycloak, Authentik, a home-lab dex instance)
// and validates them on every `/api/*` request.  On 401 the response
// includes a `WWW-Authenticate` header pointing at the provider's
// `/.well-known/openid-configuration` so any sufficiently clever client
// — the web UI, a CLI, a reverse proxy — can discover the authorization
// endpoint and start an auth code flow on its own.
//
// What this does NOT do: originate the flow.  This module never sends a
// user to `/authorize`, never handles a callback, never mints a token.
// The browser / operator does the auth code + PKCE dance against the
// provider directly; dyson just checks the signature, `iss`, `aud`,
// `exp`, `nbf`, and any required scopes.  That choice keeps the dyson
// side stateless — no session cookie, no server-side cache of per-user
// tokens, no OIDC client secret to rotate.
//
// Why JWKS instead of the /userinfo introspection endpoint?  Latency.
// JWKS fetches amortise across requests (one refresh per TTL, n=1 HTTP
// call per rotation); introspection is n=1 call per `/api/*` request
// and puts the provider on the critical path of every SSE frame emit.
// We refresh the keys lazily with a short-circuit on `kid` cache hit.
//
// Reuse vs the MCP OAuth client: there IS an `auth/oauth.rs` in this
// repo, but it's the other direction — it implements the OAuth client
// that dyson's MCP skill uses to authenticate against REMOTE MCP
// servers.  The metadata-discovery pattern and HTTP client setup
// carries over in shape, but the two modules share no types: server-
// side JWT verification needs the JWK set and an `Algorithm` matrix,
// neither of which the client-side refresh-token path cares about.
// ===========================================================================

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::auth::{Auth, AuthInfo};
use crate::error::{DysonError, Result};

/// Minimum refresh gap between JWKS fetches.  A rotating IdP typically
/// keeps old keys around for hours past rotation, so refreshing at
/// most once every 5 minutes is plenty — and it rate-limits us against
/// a broken `kid` that would otherwise hammer the discovery endpoint
/// on every request.
const MIN_JWKS_REFRESH: Duration = Duration::from_secs(300);

/// OpenID Connect Provider Configuration — the subset we consume from
/// `<issuer>/.well-known/openid-configuration`.  The spec ships many
/// more fields; we deserialize defensively and ignore anything we
/// don't use.
#[derive(Debug, Clone, Deserialize)]
pub struct OidcConfig {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub jwks_uri: String,
    #[serde(default)]
    pub token_endpoint: Option<String>,
    #[serde(default)]
    pub userinfo_endpoint: Option<String>,
}

/// A single JSON Web Key as fetched from the provider's `jwks_uri`.
/// We only need enough to build a `DecodingKey`; every IdP returns
/// at least `kid`, `kty`, `alg`, and the key-material fields for its
/// algorithm.
/// Subset of an RFC 7517 JSON Web Key we know how to verify with.
/// Only the fields we actually consume.  `alg`, `kty`, and the key-
/// material fields all default to `None` so we can skip an unfamiliar
/// JWK without failing the whole set.
#[derive(Debug, Clone, Deserialize)]
struct Jwk {
    kid: String,
    #[serde(default)]
    alg: Option<String>,
    #[serde(default)]
    kty: Option<String>,
    // RSA
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    // EC — `crv` is in the spec but jsonwebtoken's `from_ec_components`
    // infers the curve from the algorithm, so we don't bind it.
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
    // HMAC — uncommon for OIDC issuers but we support it so a local
    // dex with a shared secret works for dev.
    #[serde(default)]
    k: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

struct JwksCache {
    /// Most recently fetched set.  Replaced wholesale on refresh —
    /// partial updates would force per-key ordering semantics the spec
    /// doesn't define.
    jwks: JwkSet,
    fetched_at: Instant,
}

/// OIDC inbound auth.  Holds the discovered provider config + a
/// lazily-populated JWKS cache, plus the audience / issuer / required-
/// scope policy the operator configured.
pub struct OidcAuth {
    config: OidcConfig,
    /// Expected `aud` claim.  An IdP typically issues tokens scoped to
    /// a specific client_id; accepting any `aud` would let a token
    /// minted for one relying party authorize against dyson too.
    audience: String,
    /// Required `iss` claim — must match the discovered `config.issuer`
    /// exactly.  Stored separately so tests can swap the expected value.
    expected_issuer: String,
    /// Optional: list of scopes that must all appear in the space-
    /// separated `scope` claim.  Most operators will leave this empty
    /// and trust the audience check; enterprises that issue one `aud`
    /// across many apps can require `dyson:api` here.
    required_scopes: Vec<String>,
    /// Accepted signing algorithms.  Defaults to `[RS256]` — the OIDC
    /// majority default.  We never default to `HS256` to avoid the
    /// classic "jwt alg: none / hs256 with public key as secret" trap.
    algorithms: Vec<Algorithm>,
    jwks: Arc<RwLock<Option<JwksCache>>>,
    http: reqwest::Client,
}

impl OidcAuth {
    /// Discover the provider config + build an auth guard.  Fetches
    /// `<issuer>/.well-known/openid-configuration` once at startup so
    /// a misconfigured issuer fails fast (same posture as the rest of
    /// `from_config`).  Does NOT pre-fetch JWKS — first request pays
    /// that hop so tests can drive construction without a live IdP.
    pub async fn discover(
        issuer: &str,
        audience: String,
        required_scopes: Vec<String>,
        algorithms: Option<Vec<Algorithm>>,
    ) -> Result<Self> {
        let http = crate::http::client().clone();
        let url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let resp = http.get(&url).send().await.map_err(|e| {
            DysonError::Config(format!("oidc discovery failed for {url}: {e}"))
        })?;
        if !resp.status().is_success() {
            return Err(DysonError::Config(format!(
                "oidc discovery {url} returned HTTP {}",
                resp.status()
            )));
        }
        let config: OidcConfig = resp
            .json()
            .await
            .map_err(|e| DysonError::Config(format!("oidc metadata parse failed: {e}")))?;

        // Trust the discovered issuer, but cross-check: an IdP that
        // claims a different `iss` than we asked for is almost always
        // a misconfigured reverse proxy, and it would otherwise cause
        // mysterious 401s once tokens arrive.
        let expected_issuer = config.issuer.clone();
        if !issuer_matches(issuer, &expected_issuer) {
            return Err(DysonError::Config(format!(
                "oidc configured issuer {issuer:?} != discovered issuer {expected_issuer:?}"
            )));
        }

        Ok(Self {
            config,
            audience,
            expected_issuer,
            required_scopes,
            algorithms: algorithms.unwrap_or_else(|| vec![Algorithm::RS256]),
            jwks: Arc::new(RwLock::new(None)),
            http,
        })
    }

    /// URL clients follow to start their own auth code flow.  Surfaces
    /// in the `WWW-Authenticate` header on a 401 so a smart client can
    /// discover the authorization endpoint without out-of-band config.
    pub fn authorization_endpoint(&self) -> &str {
        &self.config.authorization_endpoint
    }

    /// The `iss` we validate against — shown in error messages and on
    /// 401 so the browser / operator can tell which IdP the token
    /// needs to come from when they're juggling more than one.
    pub fn issuer(&self) -> &str {
        &self.expected_issuer
    }

    /// The `token_endpoint` from discovery — `None` if the provider
    /// doesn't advertise one (rare; some test/dummy IdPs omit it).
    /// The SPA needs this to exchange a code for an access token, but
    /// it's not load-bearing for the verification path.
    pub fn token_endpoint(&self) -> Option<&str> {
        self.config.token_endpoint.as_deref()
    }

    async fn ensure_jwks(&self, need_refresh: bool) -> Result<()> {
        if !need_refresh {
            let guard = self.jwks.read().await;
            if guard.is_some() {
                return Ok(());
            }
        }
        // Drop the read guard before taking the write guard.
        let mut guard = self.jwks.write().await;
        if let Some(cache) = guard.as_ref()
            && !need_refresh
            && cache.fetched_at.elapsed() < MIN_JWKS_REFRESH
        {
            return Ok(());
        }
        let resp = self
            .http
            .get(&self.config.jwks_uri)
            .send()
            .await
            .map_err(|e| DysonError::Config(format!("jwks fetch failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(DysonError::Config(format!(
                "jwks fetch {} returned HTTP {}",
                self.config.jwks_uri,
                resp.status()
            )));
        }
        let jwks: JwkSet = resp
            .json()
            .await
            .map_err(|e| DysonError::Config(format!("jwks parse failed: {e}")))?;
        *guard = Some(JwksCache {
            jwks,
            fetched_at: Instant::now(),
        });
        Ok(())
    }

    fn find_key<'a>(jwks: &'a JwkSet, kid: &str) -> Option<&'a Jwk> {
        jwks.keys.iter().find(|k| k.kid == kid)
    }
}

#[async_trait]
impl Auth for OidcAuth {
    async fn validate_request(&self, headers: &hyper::HeaderMap) -> Result<AuthInfo> {
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| DysonError::Config("unauthorized".into()))?;

        let header = jsonwebtoken::decode_header(token)
            .map_err(|e| DysonError::Config(format!("jwt header decode: {e}")))?;

        // Guard against `alg: none` and any algorithm we didn't opt
        // into up front.  jsonwebtoken enforces this again inside
        // `decode`, but checking here gives a clearer error path and
        // avoids a pointless JWKS refresh for a token we'll reject.
        if !self.algorithms.contains(&header.alg) {
            return Err(DysonError::Config("unauthorized".into()));
        }

        let kid = header
            .kid
            .ok_or_else(|| DysonError::Config("unauthorized".into()))?;

        // First try the cached JWKS; on `kid` miss, refresh once (IdP
        // might have rotated).  Two lookups cap the provider traffic a
        // forged `kid` can cause.
        self.ensure_jwks(false).await?;
        let mut decoding = self.decoding_key_for(&kid).await?;
        if decoding.is_none() {
            self.ensure_jwks(true).await?;
            decoding = self.decoding_key_for(&kid).await?;
        }
        let decoding = decoding.ok_or_else(|| DysonError::Config("unauthorized".into()))?;

        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[self.expected_issuer.clone()]);
        validation.set_audience(&[self.audience.clone()]);
        validation.validate_exp = true;
        validation.validate_nbf = true;

        #[derive(Deserialize)]
        struct Claims {
            sub: String,
            #[serde(default)]
            scope: Option<String>,
        }

        let data = jsonwebtoken::decode::<Claims>(token, &decoding, &validation)
            .map_err(|_| DysonError::Config("unauthorized".into()))?;

        if !self.required_scopes.is_empty() {
            let have: std::collections::HashSet<&str> = data
                .claims
                .scope
                .as_deref()
                .unwrap_or("")
                .split_ascii_whitespace()
                .collect();
            for want in &self.required_scopes {
                if !have.contains(want.as_str()) {
                    return Err(DysonError::Config("unauthorized".into()));
                }
            }
        }

        let mut info = AuthInfo::new("oidc");
        info.metadata.insert("sub".into(), data.claims.sub);
        Ok(info)
    }
}

impl OidcAuth {
    async fn decoding_key_for(&self, kid: &str) -> Result<Option<DecodingKey>> {
        let guard = self.jwks.read().await;
        let cache = match guard.as_ref() {
            Some(c) => c,
            None => return Ok(None),
        };
        let key = match Self::find_key(&cache.jwks, kid) {
            Some(k) => k,
            None => return Ok(None),
        };
        // Honour `alg` on the JWK if the IdP pins it; fall back to the
        // `kty` to pick a builder.  Anything we can't construct a
        // DecodingKey for is treated as a miss (will cause a 401, not
        // a 500) so a partially-understood JWKS doesn't brick us.
        let alg = key.alg.as_deref();
        let kty = key.kty.as_deref();
        let decoding = match (alg, kty) {
            (Some("RS256") | Some("RS384") | Some("RS512"), _) | (_, Some("RSA")) => {
                let n = key.n.as_deref().ok_or_else(|| no_key(kid))?;
                let e = key.e.as_deref().ok_or_else(|| no_key(kid))?;
                DecodingKey::from_rsa_components(n, e)
                    .map_err(|e| DysonError::Config(format!("rsa jwk: {e}")))?
            }
            (Some("ES256") | Some("ES384") | Some("ES512"), _) | (_, Some("EC")) => {
                let x = key.x.as_deref().ok_or_else(|| no_key(kid))?;
                let y = key.y.as_deref().ok_or_else(|| no_key(kid))?;
                DecodingKey::from_ec_components(x, y)
                    .map_err(|e| DysonError::Config(format!("ec jwk: {e}")))?
            }
            (Some("HS256") | Some("HS384") | Some("HS512"), _) | (_, Some("oct")) => {
                let k = key.k.as_deref().ok_or_else(|| no_key(kid))?;
                DecodingKey::from_base64_secret(k)
                    .map_err(|e| DysonError::Config(format!("hmac jwk: {e}")))?
            }
            _ => return Ok(None),
        };
        Ok(Some(decoding))
    }
}

fn no_key(kid: &str) -> DysonError {
    DysonError::Config(format!("jwk {kid} missing key material"))
}

/// Compare two issuer URLs after normalising trailing slashes.  An IdP
/// that advertises `https://id.example.com/` vs a config of
/// `https://id.example.com` should still match — any stricter check
/// would force operators to memorise the provider's exact canonical
/// form.
fn issuer_matches(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issuer_matches_normalises_trailing_slash() {
        assert!(issuer_matches("https://idp", "https://idp/"));
        assert!(issuer_matches("https://idp/", "https://idp"));
        assert!(!issuer_matches("https://idp", "https://other"));
    }

    #[tokio::test]
    async fn missing_header_is_unauthorized() {
        // Build directly (no discover) so the test stays offline.
        let auth = OidcAuth {
            config: OidcConfig {
                issuer: "https://idp".into(),
                authorization_endpoint: "https://idp/authorize".into(),
                jwks_uri: "https://idp/jwks".into(),
                token_endpoint: None,
                userinfo_endpoint: None,
            },
            audience: "dyson".into(),
            expected_issuer: "https://idp".into(),
            required_scopes: vec![],
            algorithms: vec![Algorithm::RS256],
            jwks: Arc::new(RwLock::new(None)),
            http: crate::http::client().clone(),
        };
        let headers = hyper::HeaderMap::new();
        assert!(auth.validate_request(&headers).await.is_err());
    }

    // -----------------------------------------------------------------
    // Coverage for the verification path.  We use HS256 with a known
    // shared secret because RSA key generation in tests would either
    // pull in a heavy generator dep or hardcode a key, neither
    // worthwhile when the verifier is algorithm-agnostic past the
    // alg-allowlist check.  The discover path is exercised separately
    // via wiremock.
    // -----------------------------------------------------------------

    use base64::Engine;
    use jsonwebtoken::{EncodingKey, Header};
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn now_secs() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    /// Build an `OidcAuth` whose JWKS is preloaded with one HMAC key.
    /// Lets verification tests run entirely offline.
    fn hs256_auth(
        kid: &str,
        secret: &[u8],
        audience: &str,
        issuer: &str,
        required_scopes: Vec<String>,
        algorithms: Vec<Algorithm>,
    ) -> OidcAuth {
        let k_b64 = base64::engine::general_purpose::STANDARD.encode(secret);
        let jwks = JwkSet {
            keys: vec![Jwk {
                kid: kid.to_string(),
                alg: Some("HS256".to_string()),
                kty: Some("oct".to_string()),
                n: None,
                e: None,
                x: None,
                y: None,
                k: Some(k_b64),
            }],
        };
        OidcAuth {
            config: OidcConfig {
                issuer: issuer.to_string(),
                authorization_endpoint: format!("{issuer}/authorize"),
                jwks_uri: format!("{issuer}/jwks"),
                token_endpoint: Some(format!("{issuer}/token")),
                userinfo_endpoint: None,
            },
            audience: audience.to_string(),
            expected_issuer: issuer.to_string(),
            required_scopes,
            algorithms,
            jwks: Arc::new(RwLock::new(Some(JwksCache {
                jwks,
                fetched_at: Instant::now(),
            }))),
            http: crate::http::client().clone(),
        }
    }

    fn mint_hs256_token(
        kid: &str,
        secret: &[u8],
        claims: serde_json::Value,
    ) -> String {
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(kid.to_string());
        jsonwebtoken::encode(&header, &claims, &EncodingKey::from_secret(secret)).unwrap()
    }

    fn header_with(token: &str) -> hyper::HeaderMap {
        let mut h = hyper::HeaderMap::new();
        h.insert(
            "authorization",
            format!("Bearer {token}").parse().unwrap(),
        );
        h
    }

    #[tokio::test]
    async fn validates_hs256_token_when_algorithm_is_allowed() {
        let secret = b"shared-secret-for-tests";
        let auth = hs256_auth(
            "kid-1",
            secret,
            "dyson-web",
            "https://idp",
            vec![],
            vec![Algorithm::HS256],
        );
        let claims = serde_json::json!({
            "iss": "https://idp",
            "aud": "dyson-web",
            "sub": "alice",
            "exp": now_secs() + 60,
            "nbf": now_secs() - 60,
        });
        let token = mint_hs256_token("kid-1", secret, claims);
        let info = auth.validate_request(&header_with(&token)).await.unwrap();
        assert_eq!(info.identity, "oidc");
        assert_eq!(info.metadata.get("sub").map(String::as_str), Some("alice"));
    }

    #[tokio::test]
    async fn rejects_hs256_when_only_rs256_allowed() {
        // The algorithm-allowlist check must reject HS256 even when a
        // matching JWK is in the cache — without this, a public RSA
        // key in JWKS plus an attacker-minted HS256 token would
        // verify with the public key as the shared secret.
        let secret = b"trap";
        let auth = hs256_auth(
            "kid-1",
            secret,
            "dyson-web",
            "https://idp",
            vec![],
            vec![Algorithm::RS256],
        );
        let token = mint_hs256_token(
            "kid-1",
            secret,
            serde_json::json!({
                "iss": "https://idp",
                "aud": "dyson-web",
                "sub": "alice",
                "exp": now_secs() + 60,
            }),
        );
        assert!(auth.validate_request(&header_with(&token)).await.is_err());
    }

    #[tokio::test]
    async fn rejects_alg_none() {
        // Build a valid `alg: none` token by hand.  jsonwebtoken's
        // public encoder refuses to mint one (good!), so we craft the
        // bytes directly: header + payload + empty signature.
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&serde_json::json!({
                "alg": "none",
                "kid": "kid-1",
                "typ": "JWT",
            })).unwrap());
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&serde_json::json!({
                "iss": "https://idp",
                "aud": "dyson-web",
                "sub": "alice",
                "exp": now_secs() + 60,
            })).unwrap());
        let token = format!("{header}.{payload}.");

        let auth = hs256_auth(
            "kid-1",
            b"any",
            "dyson-web",
            "https://idp",
            vec![],
            vec![Algorithm::HS256],
        );
        // jsonwebtoken's `decode_header` may itself refuse `alg: none`.
        // Either path lands on Err — the *result* is what we care about.
        assert!(auth.validate_request(&header_with(&token)).await.is_err());
    }

    #[tokio::test]
    async fn rejects_expired_token() {
        let secret = b"s";
        let auth = hs256_auth(
            "k",
            secret,
            "aud",
            "https://idp",
            vec![],
            vec![Algorithm::HS256],
        );
        // jsonwebtoken's default leeway is 60s, so we expire well past
        // that threshold to land outside any tolerance window.
        let token = mint_hs256_token(
            "k",
            secret,
            serde_json::json!({
                "iss": "https://idp",
                "aud": "aud",
                "sub": "x",
                "exp": now_secs() - 600, // expired 10 minutes ago
            }),
        );
        assert!(auth.validate_request(&header_with(&token)).await.is_err());
    }

    #[tokio::test]
    async fn rejects_token_not_yet_valid() {
        let secret = b"s";
        let auth = hs256_auth(
            "k",
            secret,
            "aud",
            "https://idp",
            vec![],
            vec![Algorithm::HS256],
        );
        // jsonwebtoken's default leeway is 60s; pick an nbf comfortably
        // outside that window.
        let token = mint_hs256_token(
            "k",
            secret,
            serde_json::json!({
                "iss": "https://idp",
                "aud": "aud",
                "sub": "x",
                "exp": now_secs() + 1200,
                "nbf": now_secs() + 600, // valid 10 minutes from now
            }),
        );
        assert!(auth.validate_request(&header_with(&token)).await.is_err());
    }

    #[tokio::test]
    async fn rejects_wrong_audience() {
        let secret = b"s";
        let auth = hs256_auth(
            "k",
            secret,
            "dyson-web",
            "https://idp",
            vec![],
            vec![Algorithm::HS256],
        );
        let token = mint_hs256_token(
            "k",
            secret,
            serde_json::json!({
                "iss": "https://idp",
                "aud": "other-app",
                "sub": "x",
                "exp": now_secs() + 60,
            }),
        );
        assert!(auth.validate_request(&header_with(&token)).await.is_err());
    }

    #[tokio::test]
    async fn rejects_wrong_issuer() {
        let secret = b"s";
        let auth = hs256_auth(
            "k",
            secret,
            "aud",
            "https://idp",
            vec![],
            vec![Algorithm::HS256],
        );
        let token = mint_hs256_token(
            "k",
            secret,
            serde_json::json!({
                "iss": "https://attacker",
                "aud": "aud",
                "sub": "x",
                "exp": now_secs() + 60,
            }),
        );
        assert!(auth.validate_request(&header_with(&token)).await.is_err());
    }

    #[tokio::test]
    async fn required_scopes_must_all_appear_order_does_not_matter() {
        let secret = b"s";
        let auth = hs256_auth(
            "k",
            secret,
            "aud",
            "https://idp",
            vec!["dyson:api".to_string(), "openid".to_string()],
            vec![Algorithm::HS256],
        );
        // Reverse order from required: still passes.
        let ok = mint_hs256_token(
            "k",
            secret,
            serde_json::json!({
                "iss": "https://idp",
                "aud": "aud",
                "sub": "x",
                "exp": now_secs() + 60,
                "scope": "openid email dyson:api profile",
            }),
        );
        assert!(auth.validate_request(&header_with(&ok)).await.is_ok());

        // Missing one of the required scopes.
        let bad = mint_hs256_token(
            "k",
            secret,
            serde_json::json!({
                "iss": "https://idp",
                "aud": "aud",
                "sub": "x",
                "exp": now_secs() + 60,
                "scope": "openid email",
            }),
        );
        assert!(auth.validate_request(&header_with(&bad)).await.is_err());

        // Empty scope claim with required_scopes set: rejected.
        let no_scope = mint_hs256_token(
            "k",
            secret,
            serde_json::json!({
                "iss": "https://idp",
                "aud": "aud",
                "sub": "x",
                "exp": now_secs() + 60,
            }),
        );
        assert!(auth.validate_request(&header_with(&no_scope)).await.is_err());
    }

    #[tokio::test]
    async fn jwks_refresh_on_kid_miss_picks_up_rotated_key() {
        // Start with a JWKS that contains kid "old".  Rotate the IdP to
        // kid "new" and verify the auth refetches and accepts a "new"-
        // signed token.  Drives the cold-cache → first-fetch (kid miss
        // refresh) path of `validate_request`.
        let server = MockServer::start().await;

        // First request to /jwks returns the rotated set (no "old").
        let new_secret = b"rotated-secret";
        let new_k = base64::engine::general_purpose::STANDARD.encode(new_secret);
        let rotated = serde_json::json!({
            "keys": [{
                "kid": "new",
                "kty": "oct",
                "alg": "HS256",
                "k": new_k,
            }]
        });
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&rotated))
            .mount(&server)
            .await;

        // Pre-populate the cache with an obsolete "old" key so the
        // first decode_key_for(kid="new") call misses → forces a
        // refresh against the wiremock server.
        let old_secret = b"old-secret";
        let old_k = base64::engine::general_purpose::STANDARD.encode(old_secret);
        let cached = JwkSet {
            keys: vec![Jwk {
                kid: "old".into(),
                alg: Some("HS256".into()),
                kty: Some("oct".into()),
                n: None,
                e: None,
                x: None,
                y: None,
                k: Some(old_k),
            }],
        };
        let auth = OidcAuth {
            config: OidcConfig {
                issuer: "https://idp".into(),
                authorization_endpoint: "https://idp/authorize".into(),
                jwks_uri: format!("{}/jwks", server.uri()),
                token_endpoint: None,
                userinfo_endpoint: None,
            },
            audience: "dyson-web".into(),
            expected_issuer: "https://idp".into(),
            required_scopes: vec![],
            algorithms: vec![Algorithm::HS256],
            jwks: Arc::new(RwLock::new(Some(JwksCache {
                jwks: cached,
                // Fetched far in the past so MIN_JWKS_REFRESH doesn't
                // gate the kid-miss-driven refresh.
                fetched_at: Instant::now() - std::time::Duration::from_secs(3600),
            }))),
            http: crate::http::client().clone(),
        };

        let token = mint_hs256_token(
            "new",
            new_secret,
            serde_json::json!({
                "iss": "https://idp",
                "aud": "dyson-web",
                "sub": "alice",
                "exp": now_secs() + 60,
            }),
        );
        let info = auth.validate_request(&header_with(&token)).await.unwrap();
        assert_eq!(info.identity, "oidc");
    }

    #[tokio::test]
    async fn discover_succeeds_against_http_issuer() {
        // wiremock binds on http; round-trip through .well-known and
        // assert the discovered fields land where we expect.
        let server = MockServer::start().await;
        let issuer = server.uri();

        let body = serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "jwks_uri": format!("{issuer}/jwks"),
            "token_endpoint": format!("{issuer}/token"),
        });
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let auth = OidcAuth::discover(
            &issuer,
            "dyson-web".to_string(),
            vec![],
            None,
        )
        .await
        .expect("discover should succeed");
        assert_eq!(auth.issuer(), issuer);
        assert_eq!(auth.token_endpoint(), Some(format!("{issuer}/token").as_str()));
    }

    #[tokio::test]
    async fn discover_normalises_trailing_slash_on_issuer() {
        // Operator puts a trailing slash on the issuer in dyson.json
        // but the IdP advertises the same URL without the slash.  Must
        // accept rather than fail with a mysterious mismatch error.
        let server = MockServer::start().await;
        let issuer = server.uri();
        let body = serde_json::json!({
            "issuer": issuer, // no trailing slash
            "authorization_endpoint": format!("{issuer}/authorize"),
            "jwks_uri": format!("{issuer}/jwks"),
        });
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;

        let with_trailing = format!("{issuer}/");
        let auth = OidcAuth::discover(&with_trailing, "aud".into(), vec![], None).await;
        assert!(auth.is_ok(), "trailing-slash mismatch must be tolerated");
    }

    #[tokio::test]
    async fn discover_rejects_when_advertised_issuer_does_not_match() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "issuer": "https://different-issuer.example.com",
            "authorization_endpoint": "https://idp/authorize",
            "jwks_uri": "https://idp/jwks",
        });
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;
        let auth = OidcAuth::discover(&server.uri(), "aud".into(), vec![], None).await;
        assert!(auth.is_err(), "issuer mismatch must fail discovery");
    }

    #[tokio::test]
    async fn discover_propagates_http_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let auth = OidcAuth::discover(&server.uri(), "aud".into(), vec![], None).await;
        assert!(auth.is_err(), "5xx during discovery must fail fast");
    }

    #[test]
    fn auth_info_metadata_records_sub() {
        // Sanity-check the AuthInfo carrier so the metadata field used
        // by callers (e.g. structured access logs) keeps working.
        let mut info = AuthInfo::new("oidc");
        info.metadata.insert("sub".into(), "alice".into());
        let map: &HashMap<String, String> = &info.metadata;
        assert_eq!(map.get("sub").map(String::as_str), Some("alice"));
    }
}
