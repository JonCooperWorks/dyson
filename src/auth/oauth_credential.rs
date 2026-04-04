// ===========================================================================
// OAuthAuth — Auth trait implementation for OAuth 2.0 Bearer tokens with
// automatic refresh.
//
// LEARNING OVERVIEW
//
// What this module does:
//   Wraps an OAuth 2.0 access token behind the `Auth` trait so that
//   `HttpTransport` can use it identically to `BearerTokenAuth` or
//   `StaticHeadersAuth`.  The key difference: when the token expires,
//   `apply_to_request()` automatically refreshes it using the stored
//   refresh token — no caller intervention needed.
//
// How it fits in the system:
//   1. MCP skill orchestration (src/skill/mcp/mod.rs) creates an OAuthAuth
//      after completing the OAuth flow or loading persisted tokens.
//   2. HttpTransport stores it as `Box<dyn Auth>` and calls
//      `apply_to_request()` on every outgoing request.
//   3. If the access token has expired, the RwLock upgrades to a write lock
//      and refreshes inline before applying the header.
//   4. If a 401 is received despite a non-expired token (clock skew, server
//      revocation), transport.rs calls `on_unauthorized()` to force-refresh.
//
// Memory safety:
//   All token values are wrapped in `Credential` (zeroize-on-drop).
//   The `OAuthCredential` struct is behind `Arc<RwLock<>>` so concurrent
//   requests share one set of tokens and refresh atomically.
//
// Token persistence:
//   Tokens are persisted to `~/.dyson/tokens/<server_name>.json` so that
//   restarting Dyson doesn't require re-authorization.  The file is
//   created with restrictive permissions (0o600 on Unix).
// ===========================================================================

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use super::credential::Credential;
use super::oauth;
use super::Auth;
use crate::error::{DysonError, Result};

// ---------------------------------------------------------------------------
// OAuthCredential — the mutable token state behind the RwLock.
// ---------------------------------------------------------------------------

/// Mutable OAuth token state.
///
/// Held behind `Arc<RwLock<>>` inside `OAuthAuth`.  All secret values use
/// `Credential` for zeroize-on-drop memory safety.
pub struct OAuthCredential {
    /// The current access token (sent as `Authorization: Bearer <token>`).
    pub access_token: Credential,

    /// Refresh token for obtaining new access tokens.  `None` if the server
    /// didn't issue one (rare but possible).
    pub refresh_token: Option<Credential>,

    /// When the access token expires.  Set to `SystemTime::UNIX_EPOCH` if
    /// unknown (forces refresh on first use).
    pub expires_at: SystemTime,

    /// Token endpoint URL for refresh requests.
    pub token_url: String,

    /// OAuth client ID.
    pub client_id: String,

    /// Optional client secret (confidential clients only).
    pub client_secret: Option<Credential>,
}

impl OAuthCredential {
    /// Whether the access token has expired (with a 30-second safety margin).
    ///
    /// The margin prevents using a token that's about to expire — by the time
    /// the request reaches the server, it might already be invalid.
    pub fn is_expired(&self) -> bool {
        let margin = Duration::from_secs(30);
        SystemTime::now()
            .duration_since(self.expires_at)
            .map(|elapsed| elapsed > Duration::ZERO)
            .unwrap_or(false)
            || self
                .expires_at
                .duration_since(SystemTime::now())
                .map(|remaining| remaining < margin)
                .unwrap_or(true)
    }
}

// ---------------------------------------------------------------------------
// OAuthAuth — the Auth trait implementation.
// ---------------------------------------------------------------------------

/// OAuth 2.0 Bearer token authentication with automatic refresh.
///
/// Wraps `OAuthCredential` behind `Arc<RwLock<>>` so multiple concurrent
/// requests share one token set.  When the token expires, the first request
/// to notice acquires a write lock and refreshes; subsequent requests wait
/// for the refresh to complete and then use the new token.
///
/// ## Usage
///
/// ```ignore
/// let auth = OAuthAuth::new(credential);
/// let transport = HttpTransport::new(url, Box::new(auth));
/// // transport.send_request() now automatically includes OAuth Bearer token
/// // and refreshes it when expired.
/// ```
pub struct OAuthAuth {
    /// Shared mutable token state.
    credential: Arc<RwLock<OAuthCredential>>,

    /// HTTP client for refresh requests.
    http_client: reqwest::Client,
}

impl OAuthAuth {
    /// Create a new OAuthAuth from an existing credential.
    pub fn new(credential: OAuthCredential) -> Self {
        Self {
            credential: Arc::new(RwLock::new(credential)),
            http_client: reqwest::Client::new(),
        }
    }

    /// Create from a `TokenResponse` and the metadata needed for future refreshes.
    pub fn from_token_response(
        response: &oauth::TokenResponse,
        token_url: String,
        client_id: String,
        client_secret: Option<String>,
    ) -> Self {
        let expires_at = response
            .expires_in
            .map(|secs| SystemTime::now() + Duration::from_secs(secs))
            .unwrap_or(SystemTime::UNIX_EPOCH);

        let credential = OAuthCredential {
            access_token: Credential::new(response.access_token.clone()),
            refresh_token: response
                .refresh_token
                .as_ref()
                .map(|t| Credential::new(t.clone())),
            expires_at,
            token_url,
            client_id,
            client_secret: client_secret.map(Credential::new),
        };

        Self::new(credential)
    }

    /// Get a reference to the shared credential for persistence.
    pub fn credential(&self) -> &Arc<RwLock<OAuthCredential>> {
        &self.credential
    }

    /// Perform a token refresh, updating the credential in place.
    ///
    /// Called when:
    /// - `apply_to_request` detects the token has expired
    /// - `on_unauthorized` is called after a 401 response
    async fn do_refresh(cred: &RwLock<OAuthCredential>, client: &reqwest::Client) -> Result<()> {
        let mut guard = cred.write().await;

        // Double-check: another request may have already refreshed while we
        // were waiting for the write lock.
        if !guard.is_expired() {
            return Ok(());
        }

        let refresh_tok = guard.refresh_token.as_ref().ok_or_else(|| {
            DysonError::oauth(
                &guard.token_url,
                "access token expired and no refresh token available — re-authorization required",
            )
        })?;

        let response = oauth::refresh_token(
            &guard.token_url,
            refresh_tok.expose(),
            &guard.client_id,
            guard.client_secret.as_ref().map(|c| c.expose()),
            client,
        )
        .await?;

        // Update the credential with the new tokens.
        guard.access_token = Credential::new(response.access_token);
        guard.expires_at = response
            .expires_in
            .map(|secs| SystemTime::now() + Duration::from_secs(secs))
            .unwrap_or(SystemTime::UNIX_EPOCH);

        // Some servers rotate refresh tokens — use the new one if provided.
        if let Some(new_refresh) = response.refresh_token {
            guard.refresh_token = Some(Credential::new(new_refresh));
        }

        Ok(())
    }
}

#[async_trait]
impl Auth for OAuthAuth {
    /// Apply OAuth Bearer token to an outgoing request.
    ///
    /// If the token has expired, refreshes it first (blocking other requests
    /// on the write lock until refresh completes).  This is transparent to
    /// the caller — `HttpTransport` just calls `apply_to_request()` and gets
    /// back a request with a valid `Authorization: Bearer <token>` header.
    async fn apply_to_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder> {
        // Fast path: read lock, check expiry.
        {
            let guard = self.credential.read().await;
            if !guard.is_expired() {
                return Ok(request.header(
                    "Authorization",
                    format!("Bearer {}", guard.access_token.expose()),
                ));
            }
        }
        // Token expired — drop read lock before acquiring write lock to
        // avoid deadlock.

        // Slow path: refresh, then apply.
        Self::do_refresh(&self.credential, &self.http_client).await?;

        let guard = self.credential.read().await;
        Ok(request.header(
            "Authorization",
            format!("Bearer {}", guard.access_token.expose()),
        ))
    }

    /// Handle a 401 Unauthorized response by forcing a token refresh.
    ///
    /// Called by `HttpTransport` when the server rejects a request despite
    /// the token not appearing expired (clock skew, server-side revocation,
    /// etc.).  Forces a refresh regardless of `expires_at`.
    async fn on_unauthorized(&self) -> Result<()> {
        // Force expiry so do_refresh actually refreshes.
        {
            let mut guard = self.credential.write().await;
            guard.expires_at = SystemTime::UNIX_EPOCH;
        }
        Self::do_refresh(&self.credential, &self.http_client).await
    }
}

// ---------------------------------------------------------------------------
// Token persistence — save/load tokens to disk.
// ---------------------------------------------------------------------------

/// On-disk token format.
///
/// Stored at `~/.dyson/tokens/<server_name>.json`.  Uses plain strings
/// (not `Credential`) because serde needs to serialize/deserialize them;
/// they're wrapped in `Credential` immediately after loading.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedTokens {
    access_token: String,
    refresh_token: Option<String>,
    /// Seconds since UNIX epoch when the access token expires.
    expires_at_epoch: u64,
    token_url: String,
    client_id: String,
    client_secret: Option<String>,
}

/// Persist OAuth tokens to disk for the given server.
///
/// Writes to `~/.dyson/tokens/<server_name>.json` with restrictive
/// permissions (0o600 on Unix).  Creates the directory if it doesn't exist.
///
/// Called after:
/// - Initial token exchange (authorization code → tokens)
/// - Token refresh (to persist the new access token / rotated refresh token)
pub async fn persist_tokens(server_name: &str, credential: &OAuthCredential) -> Result<()> {
    let dir = token_dir()?;
    tokio::fs::create_dir_all(&dir).await?;

    // Set directory permissions to 0o700 (owner only) on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        tokio::fs::set_permissions(&dir, perms).await?;
    }

    let expires_at_epoch = credential
        .expires_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let persisted = PersistedTokens {
        access_token: credential.access_token.expose().to_string(),
        refresh_token: credential.refresh_token.as_ref().map(|t| t.expose().to_string()),
        expires_at_epoch,
        token_url: credential.token_url.clone(),
        client_id: credential.client_id.clone(),
        client_secret: credential.client_secret.as_ref().map(|s| s.expose().to_string()),
    };

    let json = serde_json::to_string_pretty(&persisted)?;
    let path = dir.join(sanitize_filename(server_name));

    tokio::fs::write(&path, json.as_bytes()).await?;

    // Set file permissions to 0o600 (owner read/write only) on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        tokio::fs::set_permissions(&path, perms).await?;
    }

    tracing::debug!(server = server_name, path = %path.display(), "OAuth tokens persisted");
    Ok(())
}

/// Load persisted OAuth tokens for the given server.
///
/// Returns `Ok(None)` if no token file exists.  Returns `Ok(Some(...))`
/// with the loaded credential if tokens are found (even if expired — the
/// refresh token may still be valid).
pub async fn load_tokens(server_name: &str) -> Result<Option<OAuthCredential>> {
    let path = token_dir()?.join(sanitize_filename(server_name));

    let data = match tokio::fs::read_to_string(&path).await {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let persisted: PersistedTokens = serde_json::from_str(&data).map_err(|e| {
        DysonError::oauth(server_name, format!("failed to parse token file: {e}"))
    })?;

    let expires_at = SystemTime::UNIX_EPOCH + Duration::from_secs(persisted.expires_at_epoch);

    let credential = OAuthCredential {
        access_token: Credential::new(persisted.access_token),
        refresh_token: persisted.refresh_token.map(Credential::new),
        expires_at,
        token_url: persisted.token_url,
        client_id: persisted.client_id,
        client_secret: persisted.client_secret.map(Credential::new),
    };

    tracing::debug!(
        server = server_name,
        expired = credential.is_expired(),
        has_refresh = credential.refresh_token.is_some(),
        "loaded persisted OAuth tokens"
    );

    Ok(Some(credential))
}

/// Token storage directory: `~/.dyson/tokens/`.
fn token_dir() -> Result<std::path::PathBuf> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).map_err(|_| {
        DysonError::Config("cannot determine home directory for token storage".into())
    })?;
    Ok(std::path::PathBuf::from(home)
        .join(".dyson")
        .join("tokens"))
}

/// Sanitize a server name for use as a filename.
///
/// Replaces any character that isn't alphanumeric, hyphen, or underscore
/// with an underscore.  Prevents path traversal attacks.
fn sanitize_filename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{sanitized}.json")
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filename_basic() {
        assert_eq!(sanitize_filename("github-copilot"), "github-copilot.json");
        assert_eq!(sanitize_filename("my_server"), "my_server.json");
    }

    #[test]
    fn sanitize_filename_prevents_traversal() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "______etc_passwd.json");
        assert_eq!(sanitize_filename("server/name"), "server_name.json");
    }

    #[test]
    fn sanitize_filename_handles_special_chars() {
        assert_eq!(sanitize_filename("server@host:8080"), "server_host_8080.json");
    }

    #[test]
    fn credential_is_expired_when_past() {
        let cred = OAuthCredential {
            access_token: Credential::new("tok".into()),
            refresh_token: None,
            expires_at: SystemTime::UNIX_EPOCH,
            token_url: String::new(),
            client_id: String::new(),
            client_secret: None,
        };
        assert!(cred.is_expired());
    }

    #[test]
    fn credential_is_not_expired_when_future() {
        let cred = OAuthCredential {
            access_token: Credential::new("tok".into()),
            refresh_token: None,
            expires_at: SystemTime::now() + Duration::from_secs(3600),
            token_url: String::new(),
            client_id: String::new(),
            client_secret: None,
        };
        assert!(!cred.is_expired());
    }

    #[test]
    fn credential_is_expired_within_margin() {
        // 10 seconds from now is within the 30-second safety margin.
        let cred = OAuthCredential {
            access_token: Credential::new("tok".into()),
            refresh_token: None,
            expires_at: SystemTime::now() + Duration::from_secs(10),
            token_url: String::new(),
            client_id: String::new(),
            client_secret: None,
        };
        assert!(cred.is_expired());
    }

    #[test]
    fn from_token_response_sets_fields() {
        let response = oauth::TokenResponse {
            access_token: "access".into(),
            token_type: "Bearer".into(),
            expires_in: Some(3600),
            refresh_token: Some("refresh".into()),
            scope: None,
        };

        let auth = OAuthAuth::from_token_response(
            &response,
            "https://auth.example.com/token".into(),
            "client-id".into(),
            Some("client-secret".into()),
        );

        // Verify we can read the credential.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let guard = auth.credential.read().await;
            assert_eq!(guard.access_token.expose(), "access");
            assert_eq!(guard.refresh_token.as_ref().unwrap().expose(), "refresh");
            assert_eq!(guard.client_id, "client-id");
            assert_eq!(guard.client_secret.as_ref().unwrap().expose(), "client-secret");
            assert!(!guard.is_expired());
        });
    }

    #[tokio::test]
    async fn apply_adds_bearer_header() {
        let cred = OAuthCredential {
            access_token: Credential::new("test-token".into()),
            refresh_token: None,
            expires_at: SystemTime::now() + Duration::from_secs(3600),
            token_url: String::new(),
            client_id: String::new(),
            client_secret: None,
        };

        let auth = OAuthAuth::new(cred);
        let client = reqwest::Client::new();
        let req = client.post("http://localhost/test");
        let req = auth.apply_to_request(req).await.unwrap();

        let built = req.build().unwrap();
        let header = built
            .headers()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(header, "Bearer test-token");
    }

    #[tokio::test]
    async fn persist_and_load_round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Override HOME so tokens go to the temp dir.
        let _dyson_dir = tmp.path().join(".dyson").join("tokens");
        // We can't easily override HOME in a test without affecting other tests,
        // so we test the serialization/deserialization directly.

        let persisted = super::PersistedTokens {
            access_token: "access-123".into(),
            refresh_token: Some("refresh-456".into()),
            expires_at_epoch: 1700000000,
            token_url: "https://auth.example.com/token".into(),
            client_id: "cid".into(),
            client_secret: Some("csecret".into()),
        };

        let json = serde_json::to_string_pretty(&persisted).unwrap();
        let parsed: super::PersistedTokens = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.access_token, "access-123");
        assert_eq!(parsed.refresh_token.as_deref(), Some("refresh-456"));
        assert_eq!(parsed.expires_at_epoch, 1700000000);
        assert_eq!(parsed.token_url, "https://auth.example.com/token");
        assert_eq!(parsed.client_id, "cid");
        assert_eq!(parsed.client_secret.as_deref(), Some("csecret"));
    }
}
