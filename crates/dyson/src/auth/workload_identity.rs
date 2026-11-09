// ===========================================================================
// Workload identity — the pod-bound token a Swarm-managed runtime sends
// beside its `pt_` bearer.
//
// Swarm mounts a projected Kubernetes service-account token, audience
// Swarm only, at /var/run/dyson/identity/token. The kubelet rewrites it
// before it expires, so it is read on every request, never cached. It goes
// only where a `pt_` Swarm proxy bearer goes: no third-party provider ever
// sees it. Outside a Swarm pod the file is absent and nothing is sent.
// ===========================================================================

/// Header Swarm reads the token from.
pub const HEADER: &str = "x-dyson-workload-identity";

/// Where Swarm mounts the token; `DYSON_WORKLOAD_IDENTITY_FILE` overrides.
pub const DEFAULT_FILE: &str = "/var/run/dyson/identity/token";

/// Swarm proxy bearers carry this prefix.
const SWARM_BEARER_PREFIX: &str = "pt_";

fn read(path: &str) -> Option<String> {
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim();
    // A JWT is printable ASCII; anything else is not a header value.
    (!token.is_empty() && token.len() <= 8192 && token.bytes().all(|b| b.is_ascii_graphic()))
        .then(|| token.to_owned())
}

/// The current token, when `credential` is a Swarm proxy bearer (bare, or
/// as an `Authorization: Bearer` value) and the token file exists.
pub fn token_for(credential: &str) -> Option<String> {
    let bearer = credential
        .strip_prefix("Bearer ")
        .unwrap_or(credential)
        .trim();
    if !bearer.starts_with(SWARM_BEARER_PREFIX) {
        return None;
    }
    let path =
        std::env::var("DYSON_WORKLOAD_IDENTITY_FILE").unwrap_or_else(|_| DEFAULT_FILE.to_owned());
    read(&path)
}

/// `request` with the identity header added when `credential` is a Swarm
/// proxy bearer and a token is mounted.
pub fn attach(request: reqwest::RequestBuilder, credential: &str) -> reqwest::RequestBuilder {
    match token_for(credential) {
        Some(token) => request.header(HEADER, token),
        None => request,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_swarm_bearers_get_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        std::fs::write(&file, "eyJhbGciOiJSUzI1NiJ9.e30.sig\n").unwrap();
        let path = file.to_str().unwrap();
        assert_eq!(read(path).as_deref(), Some("eyJhbGciOiJSUzI1NiJ9.e30.sig"));
        assert!(read(dir.path().join("missing").to_str().unwrap()).is_none());
        std::fs::write(&file, "bad\nvalue with spaces").unwrap();
        assert!(read(path).is_none());
        // Not a Swarm bearer: never looked up, whatever is mounted.
        assert!(token_for("sk-provider-key").is_none());
        assert!(token_for("Bearer sk-provider-key").is_none());
    }
}
