// ===========================================================================
// OAuth 2.0 — Authorization Code + PKCE for MCP servers.
//
// Everything OAuth lives in this one module: PKCE generation, token
// exchange/refresh, the Auth trait impl with auto-refresh, the callback
// server, and token persistence.
//
// The MCP skill layer (src/skill/mcp/mod.rs) orchestrates the flow.
// Controllers never know OAuth exists — the auth URL appears in the
// agent's system prompt.
// ===========================================================================

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, RwLock};
use tokio::task::JoinHandle;

use super::credential::Credential;
use super::Auth;
use crate::error::{DysonError, Result};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// OAuth 2.0 Authorization Server Metadata (RFC 8414).
///
/// Discovered via `/.well-known/oauth-authorization-server`.  Unknown
/// fields are silently ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
}

/// Dynamic Client Registration request (RFC 7591).
#[derive(Debug, Clone, Serialize)]
pub struct DcrRequest {
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    #[serde(default)]
    pub response_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_method: Option<String>,
}

/// Dynamic Client Registration response (RFC 7591).
#[derive(Debug, Clone, Deserialize)]
pub struct DcrResponse {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
}

/// Token endpoint response (RFC 6749 Section 5.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// PKCE code verifier + S256 challenge pair.
#[derive(Debug, Clone)]
pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
}

// ---------------------------------------------------------------------------
// Pure functions
// ---------------------------------------------------------------------------

/// Discover OAuth metadata from `<origin>/.well-known/oauth-authorization-server`.
pub async fn discover_metadata(
    server_url: &str,
    client: &reqwest::Client,
) -> Result<AuthMetadata> {
    let base = server_url.trim_end_matches('/');
    let url = format!("{base}/.well-known/oauth-authorization-server");

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| DysonError::oauth(server_url, format!("metadata discovery failed: {e}")))?;

    if !response.status().is_success() {
        return Err(DysonError::oauth(
            server_url,
            format!("metadata discovery returned HTTP {}", response.status()),
        ));
    }

    response
        .json()
        .await
        .map_err(|e| DysonError::oauth(server_url, format!("failed to parse metadata: {e}")))
}

/// Register a client via Dynamic Client Registration (RFC 7591).
pub async fn register_client(
    registration_url: &str,
    request: &DcrRequest,
    client: &reqwest::Client,
) -> Result<DcrResponse> {
    let response = client
        .post(registration_url)
        .json(request)
        .send()
        .await
        .map_err(|e| DysonError::oauth("dcr", format!("registration failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(DysonError::oauth(
            "dcr",
            format!("registration returned HTTP {status}: {text}"),
        ));
    }

    response
        .json()
        .await
        .map_err(|e| DysonError::oauth("dcr", format!("failed to parse DCR response: {e}")))
}

/// Generate PKCE code_verifier (32 random bytes, base64url) + S256 code_challenge.
pub fn generate_pkce() -> PkceChallenge {
    let random_bytes: [u8; 32] = rand::rng().random();
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    PkceChallenge { verifier, challenge }
}

/// Build the authorization URL with all required query parameters.
pub fn build_auth_url(
    authorization_endpoint: &str,
    client_id: &str,
    scopes: &[String],
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> String {
    let mut url = reqwest::Url::parse(authorization_endpoint)
        .expect("authorization_endpoint must be a valid URL");

    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", &scopes.join(" "))
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);

    url.to_string()
}

/// Exchange an authorization code for tokens.
pub async fn exchange_code(
    token_url: &str,
    code: &str,
    verifier: &str,
    client_id: &str,
    client_secret: Option<&str>,
    redirect_uri: &str,
    client: &reqwest::Client,
) -> Result<TokenResponse> {
    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier),
    ];
    let secret_owned;
    if let Some(s) = client_secret {
        secret_owned = s.to_string();
        params.push(("client_secret", &secret_owned));
    }
    post_token_request(token_url, &params, "token exchange", client).await
}

/// Refresh an expired access token.
pub async fn refresh_token(
    token_url: &str,
    refresh_token: &str,
    client_id: &str,
    client_secret: Option<&str>,
    client: &reqwest::Client,
) -> Result<TokenResponse> {
    let mut params = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    let secret_owned;
    if let Some(s) = client_secret {
        secret_owned = s.to_string();
        params.push(("client_secret", &secret_owned));
    }
    post_token_request(token_url, &params, "token refresh", client).await
}

/// Generate a random state parameter for CSRF protection (base64url, 22 chars).
pub fn generate_state() -> String {
    let bytes: [u8; 16] = rand::rng().random();
    URL_SAFE_NO_PAD.encode(bytes)
}

async fn post_token_request(
    token_url: &str,
    params: &[(&str, &str)],
    context: &str,
    client: &reqwest::Client,
) -> Result<TokenResponse> {
    let response = client
        .post(token_url)
        .form(params)
        .send()
        .await
        .map_err(|e| DysonError::oauth(token_url, format!("{context} failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(DysonError::oauth(
            token_url,
            format!("{context} returned HTTP {status}: {body}"),
        ));
    }

    response
        .json()
        .await
        .map_err(|e| DysonError::oauth(token_url, format!("{context}: bad response: {e}")))
}

// ---------------------------------------------------------------------------
// OAuthAuth — Auth trait impl with auto-refresh
// ---------------------------------------------------------------------------

/// Mutable token state, held behind `Arc<RwLock<>>`.
pub struct OAuthCredential {
    pub access_token: Credential,
    pub refresh_token: Option<Credential>,
    /// `UNIX_EPOCH` if unknown (forces refresh on first use).
    pub expires_at: SystemTime,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: Option<Credential>,
}

impl OAuthCredential {
    /// Whether the token has expired (with 30s safety margin).
    pub fn is_expired(&self) -> bool {
        match self.expires_at.duration_since(SystemTime::now()) {
            Ok(remaining) => remaining < Duration::from_secs(30),
            Err(_) => true,
        }
    }
}

/// OAuth 2.0 Bearer token auth with automatic refresh.
pub struct OAuthAuth {
    credential: Arc<RwLock<OAuthCredential>>,
    http_client: reqwest::Client,
}

impl OAuthAuth {
    pub fn new(credential: OAuthCredential) -> Self {
        Self {
            credential: Arc::new(RwLock::new(credential)),
            http_client: reqwest::Client::new(),
        }
    }

    pub fn from_token_response(
        response: &TokenResponse,
        token_url: String,
        client_id: String,
        client_secret: Option<String>,
    ) -> Self {
        let expires_at = response
            .expires_in
            .map(|secs| SystemTime::now() + Duration::from_secs(secs))
            .unwrap_or(SystemTime::UNIX_EPOCH);

        Self::new(OAuthCredential {
            access_token: Credential::new(response.access_token.clone()),
            refresh_token: response.refresh_token.as_ref().map(|t| Credential::new(t.clone())),
            expires_at,
            token_url,
            client_id,
            client_secret: client_secret.map(Credential::new),
        })
    }

    async fn do_refresh(
        cred: &RwLock<OAuthCredential>,
        client: &reqwest::Client,
        force: bool,
    ) -> Result<()> {
        let mut guard = cred.write().await;

        if !force && !guard.is_expired() {
            return Ok(());
        }

        let refresh_tok = guard.refresh_token.as_ref().ok_or_else(|| {
            DysonError::oauth(&guard.token_url, "token expired and no refresh token")
        })?;

        let response = refresh_token(
            &guard.token_url,
            refresh_tok.expose(),
            &guard.client_id,
            guard.client_secret.as_ref().map(|c| c.expose()),
            client,
        )
        .await?;

        guard.access_token = Credential::new(response.access_token);
        guard.expires_at = response
            .expires_in
            .map(|secs| SystemTime::now() + Duration::from_secs(secs))
            .unwrap_or(SystemTime::UNIX_EPOCH);

        if let Some(new_refresh) = response.refresh_token {
            guard.refresh_token = Some(Credential::new(new_refresh));
        }

        Ok(())
    }
}

#[async_trait]
impl Auth for OAuthAuth {
    async fn apply_to_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder> {
        {
            let guard = self.credential.read().await;
            if !guard.is_expired() {
                return Ok(request.header(
                    "Authorization",
                    format!("Bearer {}", guard.access_token.expose()),
                ));
            }
        }

        Self::do_refresh(&self.credential, &self.http_client, false).await?;

        let guard = self.credential.read().await;
        Ok(request.header(
            "Authorization",
            format!("Bearer {}", guard.access_token.expose()),
        ))
    }

    async fn on_unauthorized(&self) -> Result<()> {
        Self::do_refresh(&self.credential, &self.http_client, true).await
    }
}

// ---------------------------------------------------------------------------
// Callback server
// ---------------------------------------------------------------------------

/// Start a temporary HTTP server on `127.0.0.1:0` for the OAuth redirect.
///
/// Returns `(port, task_handle, code_receiver)`.  The server validates the
/// `state` parameter, sends back an HTML success page, and delivers the
/// authorization code through the oneshot channel.
pub async fn start_callback_server(
    expected_state: &str,
    timeout: Duration,
) -> Result<(u16, JoinHandle<()>, oneshot::Receiver<String>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let (tx, rx) = oneshot::channel::<String>();
    let expected_state = expected_state.to_string();

    let handle = tokio::spawn(async move {
        let tx = Arc::new(tokio::sync::Mutex::new(Some(tx)));

        let _ = tokio::time::timeout(timeout, async {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => continue,
                };

                let spawn_state = expected_state.clone();
                let spawn_tx = tx.clone();

                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = hyper::service::service_fn(move |req| {
                        let state = spawn_state.clone();
                        let tx = spawn_tx.clone();
                        async move { handle_callback(req, &state, tx).await }
                    });
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });

                if tx.lock().await.is_none() {
                    break;
                }
            }
        })
        .await;
    });

    Ok((port, handle, rx))
}

async fn handle_callback(
    req: Request<hyper::body::Incoming>,
    expected_state: &str,
    tx: Arc<tokio::sync::Mutex<Option<oneshot::Sender<String>>>>,
) -> std::result::Result<Response<Full<Bytes>>, Infallible> {
    if req.method() != hyper::Method::GET || !req.uri().path().starts_with("/callback") {
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("Not Found")))
            .unwrap());
    }

    let query = req.uri().query().unwrap_or("");
    let params: Vec<(String, String)> = reqwest::Url::parse(&format!("http://x?{query}"))
        .map(|u| u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
        .unwrap_or_default();

    let find = |key: &str| params.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());

    if let Some(error) = find("error") {
        let desc = find("error_description").unwrap_or("unknown");
        return Ok(html_response(
            StatusCode::BAD_REQUEST,
            &format!("Authorization Failed: {error}: {desc}"),
        ));
    }

    let (Some(code), Some(state)) = (find("code"), find("state")) else {
        return Ok(html_response(StatusCode::BAD_REQUEST, "Missing code or state parameter."));
    };

    if state != expected_state {
        return Ok(html_response(StatusCode::BAD_REQUEST, "State mismatch — possible CSRF."));
    }

    if let Some(sender) = tx.lock().await.take() {
        let _ = sender.send(code.to_string());
    }

    Ok(html_response(
        StatusCode::OK,
        "Authorization complete. You can close this tab.",
    ))
}

fn html_response(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(format!(
            "<html><body><h1>{message}</h1></body></html>"
        ))))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Token persistence
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct PersistedTokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_at_epoch: u64,
    token_url: String,
    client_id: String,
    client_secret: Option<String>,
}

/// Persist tokens from a `TokenResponse` to `~/.dyson/tokens/<server>.json`.
pub async fn persist_tokens(
    server_name: &str,
    response: &TokenResponse,
    token_url: &str,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<()> {
    let expires_at = response
        .expires_in
        .map(|secs| SystemTime::now() + Duration::from_secs(secs))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let expires_at_epoch = expires_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let persisted = PersistedTokens {
        access_token: response.access_token.clone(),
        refresh_token: response.refresh_token.clone(),
        expires_at_epoch,
        token_url: token_url.to_string(),
        client_id: client_id.to_string(),
        client_secret: client_secret.map(|s| s.to_string()),
    };

    let dir = token_dir()?;
    tokio::fs::create_dir_all(&dir).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).await?;
    }

    let path = dir.join(sanitize_filename(server_name));
    tokio::fs::write(&path, serde_json::to_string_pretty(&persisted)?).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    }

    Ok(())
}

/// Load persisted tokens. Returns `None` if the file doesn't exist.
pub async fn load_tokens(server_name: &str) -> Result<Option<OAuthCredential>> {
    let path = token_dir()?.join(sanitize_filename(server_name));

    let data = match tokio::fs::read_to_string(&path).await {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let p: PersistedTokens = serde_json::from_str(&data)
        .map_err(|e| DysonError::oauth(server_name, format!("bad token file: {e}")))?;

    Ok(Some(OAuthCredential {
        access_token: Credential::new(p.access_token),
        refresh_token: p.refresh_token.map(Credential::new),
        expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(p.expires_at_epoch),
        token_url: p.token_url,
        client_id: p.client_id,
        client_secret: p.client_secret.map(Credential::new),
    }))
}

fn token_dir() -> Result<std::path::PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| DysonError::Config("HOME not set".into()))?;
    Ok(std::path::PathBuf::from(home).join(".dyson").join("tokens"))
}

fn sanitize_filename(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    format!("{s}.json")
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
        assert_eq!(pkce.verifier.len(), 43);
    }

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let pkce = generate_pkce();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn pkce_unique() {
        let a = generate_pkce();
        let b = generate_pkce();
        assert_ne!(a.verifier, b.verifier);
    }

    #[test]
    fn build_auth_url_encodes_correctly() {
        let url = build_auth_url(
            "https://auth.example.com/authorize",
            "my-client",
            &["read".into(), "write".into()],
            "http://127.0.0.1:8080/callback",
            "challenge",
            "state",
        );

        let parsed = reqwest::Url::parse(&url).unwrap();
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().collect();
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["client_id"], "my-client");
        assert_eq!(pairs["scope"], "read write");
        assert_eq!(pairs["redirect_uri"], "http://127.0.0.1:8080/callback");
    }

    #[test]
    fn build_auth_url_preserves_existing_query() {
        let url = build_auth_url(
            "https://auth.example.com/authorize?extra=1",
            "cid", &[], "http://localhost/cb", "ch", "st",
        );
        let parsed = reqwest::Url::parse(&url).unwrap();
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().collect();
        assert_eq!(pairs["extra"], "1");
        assert_eq!(pairs["response_type"], "code");
    }

    #[test]
    fn state_unique() {
        assert_ne!(generate_state(), generate_state());
        assert_eq!(generate_state().len(), 22);
    }

    #[test]
    fn token_response_deserialize() {
        let json = r#"{"access_token":"tok","token_type":"Bearer","expires_in":3600,"refresh_token":"rtok"}"#;
        let resp: TokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "tok");
        assert_eq!(resp.expires_in, Some(3600));
        assert_eq!(resp.refresh_token.as_deref(), Some("rtok"));
    }

    #[test]
    fn token_response_minimal() {
        let json = r#"{"access_token":"tok","token_type":"Bearer"}"#;
        let resp: TokenResponse = serde_json::from_str(json).unwrap();
        assert!(resp.expires_in.is_none());
        assert!(resp.refresh_token.is_none());
    }

    #[test]
    fn sanitize_filename_prevents_traversal() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "______etc_passwd.json");
    }

    #[test]
    fn credential_expiry() {
        let expired = OAuthCredential {
            access_token: Credential::new("t".into()),
            refresh_token: None,
            expires_at: SystemTime::UNIX_EPOCH,
            token_url: String::new(), client_id: String::new(), client_secret: None,
        };
        assert!(expired.is_expired());

        let valid = OAuthCredential {
            access_token: Credential::new("t".into()),
            refresh_token: None,
            expires_at: SystemTime::now() + Duration::from_secs(3600),
            token_url: String::new(), client_id: String::new(), client_secret: None,
        };
        assert!(!valid.is_expired());

        // Within 30s margin → expired
        let marginal = OAuthCredential {
            access_token: Credential::new("t".into()),
            refresh_token: None,
            expires_at: SystemTime::now() + Duration::from_secs(10),
            token_url: String::new(), client_id: String::new(), client_secret: None,
        };
        assert!(marginal.is_expired());
    }

    #[tokio::test]
    async fn apply_adds_bearer_header() {
        let auth = OAuthAuth::new(OAuthCredential {
            access_token: Credential::new("test-token".into()),
            refresh_token: None,
            expires_at: SystemTime::now() + Duration::from_secs(3600),
            token_url: String::new(), client_id: String::new(), client_secret: None,
        });
        let client = reqwest::Client::new();
        let req = auth.apply_to_request(client.post("http://localhost/test")).await.unwrap();
        let built = req.build().unwrap();
        assert_eq!(
            built.headers().get("authorization").unwrap().to_str().unwrap(),
            "Bearer test-token"
        );
    }

    #[test]
    fn persisted_tokens_round_trip() {
        let p = PersistedTokens {
            access_token: "a".into(), refresh_token: Some("r".into()),
            expires_at_epoch: 1700000000, token_url: "https://t".into(),
            client_id: "c".into(), client_secret: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        let p2: PersistedTokens = serde_json::from_str(&json).unwrap();
        assert_eq!(p2.access_token, "a");
        assert_eq!(p2.refresh_token.as_deref(), Some("r"));
    }

    #[tokio::test]
    async fn callback_server_receives_code() {
        let (port, handle, rx) = start_callback_server("my-state", Duration::from_secs(5))
            .await.unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/callback?code=abc&state=my-state"))
            .send().await.unwrap();
        assert_eq!(resp.status(), 200);

        let code = rx.await.unwrap();
        assert_eq!(code, "abc");
        handle.abort();
    }

    #[tokio::test]
    async fn callback_server_rejects_wrong_state() {
        let (port, handle, _rx) = start_callback_server("correct", Duration::from_secs(5))
            .await.unwrap();

        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/callback?code=c&state=wrong"))
            .send().await.unwrap();
        assert_eq!(resp.status(), 400);
        handle.abort();
    }

    #[tokio::test]
    async fn callback_server_404_on_wrong_path() {
        let (port, handle, _rx) = start_callback_server("s", Duration::from_secs(5))
            .await.unwrap();

        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/wrong"))
            .send().await.unwrap();
        assert_eq!(resp.status(), 404);
        handle.abort();
    }
}
