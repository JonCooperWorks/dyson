// ===========================================================================
// OAuth 2.0 primitives — pure functions for the Authorization Code + PKCE flow.
//
// LEARNING OVERVIEW
//
// What this module does:
//   Provides the building blocks for OAuth 2.0 Authorization Code with PKCE,
//   as used by MCP servers that require interactive authorization (e.g.,
//   GitHub Copilot MCP).
//
//   Every function here is pure: it takes inputs, makes an HTTP call or does
//   a computation, and returns a result.  No global state, no side effects
//   beyond the network calls.  This makes them easy to unit-test with mock
//   HTTP responses.
//
// How these fit together:
//
//   1. discover_metadata()  — learn the server's OAuth endpoints
//   2. register_client()    — (optional) Dynamic Client Registration
//   3. generate_pkce()      — create a PKCE code_verifier + code_challenge
//   4. build_auth_url()     — construct the URL the user visits to authorize
//   5. exchange_code()      — swap the authorization code for tokens
//   6. refresh_token()      — get new tokens when the access token expires
//
// PKCE (Proof Key for Code Exchange):
//   Prevents authorization code interception attacks.  The client generates
//   a random `code_verifier`, sends its SHA-256 hash (`code_challenge`) in
//   the auth request, then proves possession of the verifier during token
//   exchange.  This is required for public clients (no client_secret) and
//   recommended for all OAuth 2.0 flows per RFC 7636.
//
// Controller-agnostic design:
//   These functions don't know about controllers, agents, or UI.  The MCP
//   skill layer (src/skill/mcp/mod.rs) orchestrates the flow and surfaces
//   the auth URL through the agent's system prompt, which works identically
//   across Terminal, Telegram, and any future controller.
// ===========================================================================

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{DysonError, Result};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// OAuth 2.0 Authorization Server Metadata (RFC 8414).
///
/// Discovered from `/.well-known/oauth-authorization-server` on the MCP
/// server's origin.  Only the fields Dyson actually uses are included;
/// unknown fields are silently ignored via `#[serde(default)]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthMetadata {
    /// URL of the authorization endpoint (where the user grants access).
    pub authorization_endpoint: String,

    /// URL of the token endpoint (where codes are exchanged for tokens).
    pub token_endpoint: String,

    /// URL of the Dynamic Client Registration endpoint (RFC 7591).
    /// `None` if the server doesn't support DCR.
    #[serde(default)]
    pub registration_endpoint: Option<String>,

    /// OAuth response types the server supports (e.g., `["code"]`).
    #[serde(default)]
    pub response_types_supported: Vec<String>,

    /// PKCE code challenge methods supported (e.g., `["S256"]`).
    #[serde(default)]
    pub code_challenge_methods_supported: Vec<String>,

    /// Scopes the server supports.
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}

/// Dynamic Client Registration request (RFC 7591).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DcrRequest {
    /// Human-readable name for the client.
    pub client_name: String,

    /// Redirect URIs the client will use.
    pub redirect_uris: Vec<String>,

    /// Grant types the client will use (e.g., `["authorization_code", "refresh_token"]`).
    pub grant_types: Vec<String>,

    /// Response types the client will use (e.g., `["code"]`).
    #[serde(default)]
    pub response_types: Vec<String>,

    /// Token endpoint auth method (e.g., `"none"` for public clients).
    #[serde(default)]
    pub token_endpoint_auth_method: Option<String>,
}

/// Dynamic Client Registration response (RFC 7591).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DcrResponse {
    /// The assigned client ID.
    pub client_id: String,

    /// Optional client secret (confidential clients only).
    #[serde(default)]
    pub client_secret: Option<String>,

    /// When the client secret expires (0 = never).
    #[serde(default)]
    pub client_secret_expires_at: Option<u64>,
}

/// Token endpoint response (RFC 6749 Section 5.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    /// The access token issued by the authorization server.
    pub access_token: String,

    /// Token type (almost always "Bearer").
    pub token_type: String,

    /// Lifetime of the access token in seconds.
    #[serde(default)]
    pub expires_in: Option<u64>,

    /// Refresh token for obtaining new access tokens.
    #[serde(default)]
    pub refresh_token: Option<String>,

    /// Space-separated list of granted scopes.
    #[serde(default)]
    pub scope: Option<String>,
}

/// PKCE code verifier and challenge pair.
///
/// The `verifier` is a random string sent during token exchange.
/// The `challenge` is its SHA-256 hash (base64url-encoded) sent during
/// the authorization request.
#[derive(Debug, Clone)]
pub struct PkceChallenge {
    /// Random code verifier (base64url, 43 chars from 32 random bytes).
    pub verifier: String,
    /// S256 code challenge = base64url(SHA-256(verifier)).
    pub challenge: String,
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Discover OAuth 2.0 authorization server metadata.
///
/// Fetches `<origin>/.well-known/oauth-authorization-server` as per RFC 8414.
/// If that fails, returns an error — callers should check whether the MCP
/// server actually requires OAuth before calling this.
///
/// # Arguments
/// * `server_url` — The MCP server's base URL (e.g., `https://mcp.example.com`)
/// * `client` — A reqwest HTTP client for making the discovery request
pub async fn discover_metadata(
    server_url: &str,
    client: &reqwest::Client,
) -> Result<AuthMetadata> {
    // Strip trailing slashes to normalize the URL.
    let base = server_url.trim_end_matches('/');
    let well_known = format!("{base}/.well-known/oauth-authorization-server");

    tracing::debug!(url = %well_known, "discovering OAuth metadata");

    let response = client.get(&well_known).send().await.map_err(|e| {
        DysonError::oauth(server_url, format!("metadata discovery failed: {e}"))
    })?;

    if !response.status().is_success() {
        return Err(DysonError::oauth(
            server_url,
            format!(
                "metadata discovery returned HTTP {}",
                response.status()
            ),
        ));
    }

    let metadata: AuthMetadata = response.json().await.map_err(|e| {
        DysonError::oauth(server_url, format!("failed to parse metadata: {e}"))
    })?;

    tracing::info!(
        server = server_url,
        authorization_endpoint = %metadata.authorization_endpoint,
        token_endpoint = %metadata.token_endpoint,
        "OAuth metadata discovered"
    );

    Ok(metadata)
}

// ---------------------------------------------------------------------------
// Dynamic Client Registration
// ---------------------------------------------------------------------------

/// Register a new OAuth client via Dynamic Client Registration (RFC 7591).
///
/// # Arguments
/// * `registration_url` — The DCR endpoint from `AuthMetadata.registration_endpoint`
/// * `request` — Client metadata to register
/// * `client` — A reqwest HTTP client
pub async fn register_client(
    registration_url: &str,
    request: &DcrRequest,
    client: &reqwest::Client,
) -> Result<DcrResponse> {
    tracing::debug!(url = %registration_url, client_name = %request.client_name, "registering OAuth client");

    let response = client
        .post(registration_url)
        .json(request)
        .send()
        .await
        .map_err(|e| {
            DysonError::oauth("dcr", format!("client registration failed: {e}"))
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "failed to read body".into());
        return Err(DysonError::oauth(
            "dcr",
            format!("client registration returned HTTP {status}: {body}"),
        ));
    }

    let dcr_response: DcrResponse = response.json().await.map_err(|e| {
        DysonError::oauth("dcr", format!("failed to parse DCR response: {e}"))
    })?;

    tracing::info!(
        client_id = %dcr_response.client_id,
        has_secret = dcr_response.client_secret.is_some(),
        "OAuth client registered"
    );

    Ok(dcr_response)
}

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

/// Generate a PKCE code verifier and S256 code challenge.
///
/// Per RFC 7636:
/// - `code_verifier`: 32 random bytes, base64url-encoded (43 chars)
/// - `code_challenge`: base64url(SHA-256(code_verifier))
/// - `code_challenge_method`: always "S256"
///
/// The verifier is cryptographically random, making it infeasible for an
/// attacker to guess or brute-force.
pub fn generate_pkce() -> PkceChallenge {
    // 32 bytes of cryptographic randomness → 43-char base64url string.
    let random_bytes: [u8; 32] = rand::rng().random();
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes);

    // S256: challenge = base64url(SHA-256(ASCII(verifier)))
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);

    PkceChallenge { verifier, challenge }
}

// ---------------------------------------------------------------------------
// Authorization URL
// ---------------------------------------------------------------------------

/// Build the authorization URL the user visits to grant access.
///
/// This URL is sent to the user (via the agent's system prompt) and opened
/// in their browser.  After granting access, the authorization server
/// redirects to `redirect_uri` with an authorization code.
///
/// # Arguments
/// * `metadata` — Discovered server metadata (provides `authorization_endpoint`)
/// * `client_id` — The OAuth client ID (from config or DCR)
/// * `scopes` — Requested scopes (space-separated in the URL)
/// * `redirect_uri` — Where the auth server should redirect after authorization
/// * `code_challenge` — PKCE S256 challenge (from `generate_pkce()`)
/// * `state` — Random state parameter for CSRF protection
pub fn build_auth_url(
    metadata: &AuthMetadata,
    client_id: &str,
    scopes: &[String],
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> String {
    // Build query parameters manually to avoid adding a `url` crate dependency.
    // All values are percent-encoded where necessary.
    let scope_str = scopes.join(" ");
    let params = [
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("scope", &scope_str),
        ("code_challenge", code_challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ];

    let query: String = params
        .iter()
        .map(|(k, v)| format!("{k}={}", percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let sep = if metadata.authorization_endpoint.contains('?') {
        "&"
    } else {
        "?"
    };

    format!("{}{sep}{query}", metadata.authorization_endpoint)
}

/// Minimal percent-encoding for URL query parameter values.
///
/// Encodes characters that are not unreserved per RFC 3986:
/// unreserved = ALPHA / DIGIT / "-" / "." / "_" / "~"
fn percent_encode(input: &str) -> String {
    let mut encoded = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    encoded
}

// ---------------------------------------------------------------------------
// Token exchange
// ---------------------------------------------------------------------------

/// Exchange an authorization code for tokens.
///
/// This is the second leg of the Authorization Code flow: after the user
/// authorizes and the callback receives the code, we POST it to the token
/// endpoint along with the PKCE verifier to prove we initiated the flow.
///
/// # Arguments
/// * `token_url` — Token endpoint from `AuthMetadata.token_endpoint`
/// * `code` — Authorization code from the callback
/// * `verifier` — PKCE code verifier (proves we started the flow)
/// * `client_id` — OAuth client ID
/// * `client_secret` — Optional client secret (confidential clients)
/// * `redirect_uri` — Must match the URI used in the authorization request
/// * `client` — A reqwest HTTP client
pub async fn exchange_code(
    token_url: &str,
    code: &str,
    verifier: &str,
    client_id: &str,
    client_secret: Option<&str>,
    redirect_uri: &str,
    client: &reqwest::Client,
) -> Result<TokenResponse> {
    tracing::debug!(token_url = %token_url, "exchanging authorization code for tokens");

    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier),
    ];

    // Confidential clients include client_secret in the request body.
    // Public clients (PKCE-only) omit it.
    let secret_owned;
    if let Some(secret) = client_secret {
        secret_owned = secret.to_string();
        params.push(("client_secret", &secret_owned));
    }

    let response = client
        .post(token_url)
        .form(&params)
        .send()
        .await
        .map_err(|e| {
            DysonError::oauth(token_url, format!("token exchange failed: {e}"))
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "failed to read body".into());
        return Err(DysonError::oauth(
            token_url,
            format!("token exchange returned HTTP {status}: {body}"),
        ));
    }

    let token_response: TokenResponse = response.json().await.map_err(|e| {
        DysonError::oauth(token_url, format!("failed to parse token response: {e}"))
    })?;

    tracing::info!(
        token_type = %token_response.token_type,
        expires_in = ?token_response.expires_in,
        has_refresh = token_response.refresh_token.is_some(),
        "tokens received"
    );

    Ok(token_response)
}

// ---------------------------------------------------------------------------
// Token refresh
// ---------------------------------------------------------------------------

/// Refresh an expired access token using a refresh token.
///
/// Called automatically by `OAuthAuth::apply_to_request()` when the access
/// token has expired.  The refresh token is long-lived and stored on disk
/// at `~/.dyson/tokens/<server>.json`.
///
/// # Arguments
/// * `token_url` — Token endpoint from the original metadata
/// * `refresh_token` — The refresh token from a previous token response
/// * `client_id` — OAuth client ID
/// * `client_secret` — Optional client secret (confidential clients)
/// * `client` — A reqwest HTTP client
pub async fn refresh_token(
    token_url: &str,
    refresh_token: &str,
    client_id: &str,
    client_secret: Option<&str>,
    client: &reqwest::Client,
) -> Result<TokenResponse> {
    tracing::debug!(token_url = %token_url, "refreshing access token");

    let mut params = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];

    let secret_owned;
    if let Some(secret) = client_secret {
        secret_owned = secret.to_string();
        params.push(("client_secret", &secret_owned));
    }

    let response = client
        .post(token_url)
        .form(&params)
        .send()
        .await
        .map_err(|e| {
            DysonError::oauth(token_url, format!("token refresh failed: {e}"))
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "failed to read body".into());
        return Err(DysonError::oauth(
            token_url,
            format!("token refresh returned HTTP {status}: {body}"),
        ));
    }

    let token_response: TokenResponse = response.json().await.map_err(|e| {
        DysonError::oauth(token_url, format!("failed to parse refresh response: {e}"))
    })?;

    tracing::info!(
        expires_in = ?token_response.expires_in,
        has_new_refresh = token_response.refresh_token.is_some(),
        "access token refreshed"
    );

    Ok(token_response)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate a random state parameter for CSRF protection.
///
/// Returns a 16-byte random value encoded as base64url (22 chars).
pub fn generate_state() -> String {
    let bytes: [u8; 16] = rand::rng().random();
    URL_SAFE_NO_PAD.encode(bytes)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_length() {
        let pkce = generate_pkce();
        // 32 bytes → 43 chars in base64url (no padding).
        assert_eq!(pkce.verifier.len(), 43);
    }

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let pkce = generate_pkce();

        // Recompute: challenge should be base64url(SHA-256(verifier)).
        let digest = Sha256::digest(pkce.verifier.as_bytes());
        let expected = URL_SAFE_NO_PAD.encode(digest);

        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn pkce_challenge_length() {
        let pkce = generate_pkce();
        // SHA-256 produces 32 bytes → 43 chars in base64url.
        assert_eq!(pkce.challenge.len(), 43);
    }

    #[test]
    fn pkce_generates_unique_values() {
        let a = generate_pkce();
        let b = generate_pkce();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
    }

    #[test]
    fn build_auth_url_basic() {
        let metadata = AuthMetadata {
            authorization_endpoint: "https://auth.example.com/authorize".into(),
            token_endpoint: "https://auth.example.com/token".into(),
            registration_endpoint: None,
            response_types_supported: vec!["code".into()],
            code_challenge_methods_supported: vec!["S256".into()],
            scopes_supported: vec![],
        };

        let url = build_auth_url(
            &metadata,
            "my-client",
            &["read".into(), "write".into()],
            "http://127.0.0.1:8080/callback",
            "test-challenge",
            "test-state",
        );

        assert!(url.starts_with("https://auth.example.com/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=my-client"));
        assert!(url.contains("scope=read%20write"));
        assert!(url.contains("code_challenge=test-challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=test-state"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8080%2Fcallback"));
    }

    #[test]
    fn build_auth_url_preserves_existing_query() {
        let metadata = AuthMetadata {
            authorization_endpoint: "https://auth.example.com/authorize?extra=1".into(),
            token_endpoint: String::new(),
            registration_endpoint: None,
            response_types_supported: vec![],
            code_challenge_methods_supported: vec![],
            scopes_supported: vec![],
        };

        let url = build_auth_url(&metadata, "cid", &[], "http://localhost/cb", "ch", "st");

        // Should use & instead of ? since the endpoint already has a query string.
        assert!(url.starts_with("https://auth.example.com/authorize?extra=1&"));
    }

    #[test]
    fn state_generation() {
        let s1 = generate_state();
        let s2 = generate_state();
        // 16 bytes → 22 chars in base64url.
        assert_eq!(s1.len(), 22);
        assert_ne!(s1, s2);
    }

    #[test]
    fn percent_encode_preserves_unreserved() {
        assert_eq!(percent_encode("hello-world_123.test~"), "hello-world_123.test~");
    }

    #[test]
    fn percent_encode_encodes_special_chars() {
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("foo@bar"), "foo%40bar");
        assert_eq!(percent_encode("http://x"), "http%3A%2F%2Fx");
    }

    #[test]
    fn token_response_deserialize_minimal() {
        let json = r#"{"access_token":"tok","token_type":"Bearer"}"#;
        let resp: TokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "tok");
        assert_eq!(resp.token_type, "Bearer");
        assert!(resp.expires_in.is_none());
        assert!(resp.refresh_token.is_none());
        assert!(resp.scope.is_none());
    }

    #[test]
    fn token_response_deserialize_full() {
        let json = r#"{
            "access_token": "tok",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "rtok",
            "scope": "read write"
        }"#;
        let resp: TokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.expires_in, Some(3600));
        assert_eq!(resp.refresh_token.as_deref(), Some("rtok"));
        assert_eq!(resp.scope.as_deref(), Some("read write"));
    }

    #[test]
    fn auth_metadata_deserialize_ignores_unknown_fields() {
        let json = r#"{
            "authorization_endpoint": "https://auth/authorize",
            "token_endpoint": "https://auth/token",
            "issuer": "https://auth",
            "unknown_field": true
        }"#;
        // Should not fail despite unknown fields.
        let meta: AuthMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.authorization_endpoint, "https://auth/authorize");
        assert_eq!(meta.token_endpoint, "https://auth/token");
    }

    #[test]
    fn dcr_request_serializes_correctly() {
        let req = DcrRequest {
            client_name: "dyson".into(),
            redirect_uris: vec!["http://127.0.0.1:9999/callback".into()],
            grant_types: vec!["authorization_code".into(), "refresh_token".into()],
            response_types: vec!["code".into()],
            token_endpoint_auth_method: Some("none".into()),
        };

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["client_name"], "dyson");
        assert_eq!(json["grant_types"][0], "authorization_code");
        assert_eq!(json["token_endpoint_auth_method"], "none");
    }
}
