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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_bind_recognises_v4_v6_and_rejects_others() {
        // The whole point of this gate is to refuse to default to
        // dangerous_no_auth on a non-loopback bind.  Keep all the
        // shapes operators actually paste into dyson.json under one
        // test so a future change is forced to think about each case.
        assert!(is_loopback_bind("127.0.0.1:7878"));
        assert!(is_loopback_bind("127.0.0.99:7878"), "every 127/8 host is loopback");
        assert!(is_loopback_bind("[::1]:7878"));
        assert!(!is_loopback_bind("0.0.0.0:7878"), "0.0.0.0 is the trap we exist to catch");
        assert!(!is_loopback_bind("[::]:7878"));
        assert!(!is_loopback_bind("192.168.1.10:7878"));
        assert!(!is_loopback_bind("10.0.0.1:7878"));
        // `localhost` is intentionally not parsed — refusing it forces the
        // operator to write `127.0.0.1` rather than trust /etc/hosts.
        assert!(!is_loopback_bind("localhost:7878"));
        // Garbage at the end of the world.
        assert!(!is_loopback_bind(""));
        assert!(!is_loopback_bind("not a bind"));
    }

    #[test]
    fn config_default_bind_is_loopback() {
        // Round-trip an empty config through the deserialiser to
        // guarantee the operator who pastes `{ "type": "http" }` lands
        // on a loopback bind.
        let raw: HttpControllerConfigRaw = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(raw.bind, "127.0.0.1:7878");
        assert!(is_loopback_bind(&raw.bind));
        assert!(raw.auth.is_none(), "default auth is unset → loopback gets DangerousNoAuth");
    }

    #[test]
    fn config_parses_bearer_and_oidc_and_dangerous_variants() {
        let bearer: HttpAuthConfig = serde_json::from_value(serde_json::json!({
            "type": "bearer",
            "hash": "$argon2id$v=19$m=19456,t=2,p=1$abc$def",
        }))
        .unwrap();
        assert!(matches!(bearer, HttpAuthConfig::Bearer { .. }));

        let oidc: HttpAuthConfig = serde_json::from_value(serde_json::json!({
            "type": "oidc",
            "issuer": "https://idp.example.com",
            "audience": "dyson-web",
            "required_scopes": ["dyson:api", "openid"],
        }))
        .unwrap();
        match oidc {
            HttpAuthConfig::Oidc { issuer, audience, required_scopes } => {
                assert_eq!(issuer, "https://idp.example.com");
                assert_eq!(audience, "dyson-web");
                assert_eq!(required_scopes, vec!["dyson:api".to_string(), "openid".to_string()]);
            }
            _ => panic!("expected Oidc"),
        }

        let none: HttpAuthConfig = serde_json::from_value(serde_json::json!({
            "type": "dangerous_no_auth",
        }))
        .unwrap();
        assert!(matches!(none, HttpAuthConfig::DangerousNoAuth));

        // Unknown type tag — must fail loudly rather than silently
        // fall back to no-auth.
        let bad = serde_json::from_value::<HttpAuthConfig>(serde_json::json!({
            "type": "magic",
        }));
        assert!(bad.is_err());
    }
}
