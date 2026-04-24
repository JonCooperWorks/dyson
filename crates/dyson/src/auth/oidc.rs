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
}
