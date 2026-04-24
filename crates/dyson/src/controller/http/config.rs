// ===========================================================================
// HTTP controller — config-time DTOs.
//
// Shapes parsed out of the operator's `dyson.json` `controllers[]` entry
// for a `"type": "http"` controller.  Held briefly: `from_config` reads
// these into an `HttpController`, then they're discarded.
// ===========================================================================

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct HttpControllerConfigRaw {
    /// Address to bind, e.g. "127.0.0.1:7878".  Loopback-only by default
    /// because there is no inbound auth.
    #[serde(default = "default_bind")]
    pub(crate) bind: String,

    /// Inbound authentication mechanism.  Optional on a loopback bind
    /// (127.0.0.1 / ::1) — the loopback assumption is a single trusted
    /// operator, so a missing field defaults to `DangerousNoAuth` there.
    /// On any other bind the field is required: omitting it refuses to
    /// start the controller so you can't silently expose an
    /// unauthenticated endpoint.
    #[serde(default)]
    pub(crate) auth: Option<HttpAuthConfig>,
}

/// Which inbound auth mechanism guards the HTTP API.
///
/// `DangerousNoAuth` is the explicit opt-in to an unauthenticated
/// endpoint — the controller still starts, but logs a loud warning.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum HttpAuthConfig {
    /// No authentication.  Every request is accepted as `anonymous`.
    DangerousNoAuth,
    /// `Authorization: Bearer <token>` validated against a stored
    /// Argon2id PHC hash.  We never persist the plaintext token —
    /// operators paste a hash (`$argon2id$...`) into dyson.json and
    /// share the matching plaintext with their browser.  Generate the
    /// hash with `dyson hash-bearer`.
    Bearer { hash: String },
    /// Verify `Authorization: Bearer <jwt>` against an external OpenID
    /// Connect provider.  The controller fetches
    /// `<issuer>/.well-known/openid-configuration` at startup for the
    /// JWKS URI, then validates signature + `iss` + `aud` + `exp` +
    /// `nbf` + (optional) `scope` on every `/api/*` request.  The SPA
    /// / CLI / reverse proxy handles the auth code flow itself — on
    /// 401 we emit a `WWW-Authenticate` header pointing at the
    /// authorization endpoint so clients can discover it.
    Oidc {
        /// Base URL of the OIDC provider, e.g. `https://accounts.example.com`.
        /// `.well-known/openid-configuration` is appended automatically.
        issuer: String,
        /// Required `aud` claim.  Typically the OAuth `client_id`
        /// registered for this dyson instance.
        audience: String,
        /// Optional space-separated scopes that must all appear in the
        /// token's `scope` claim.  Use when one IdP client covers
        /// many relying parties (e.g. `dyson:api`).
        #[serde(default)]
        required_scopes: Vec<String>,
    },
}

fn default_bind() -> String {
    "127.0.0.1:7878".to_string()
}

/// True when `bind` resolves to a loopback address (`127.0.0.0/8` or
/// `::1`).  Used to gate the `auth`-field default: the loopback threat
/// model is a single trusted operator, so `DangerousNoAuth` is fine
/// there; any other bind must name a mechanism explicitly.
///
/// `localhost` is intentionally NOT treated as loopback without a DNS
/// lookup — if an operator writes `localhost:7878` they're trusting
/// `/etc/hosts`, which is a different story; safer to force them to be
/// explicit.  `0.0.0.0` / `::` are NOT loopback, which is the whole
/// point.
pub(crate) fn is_loopback_bind(bind: &str) -> bool {
    bind.parse::<std::net::SocketAddr>()
        .map(|addr| addr.ip().is_loopback())
        .unwrap_or(false)
}
