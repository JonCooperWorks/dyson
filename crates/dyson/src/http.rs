// ===========================================================================
// HTTP client — single, shared reqwest::Client for the entire process.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Provides a process-wide `reqwest::Client` singleton used by all
//   HTTP-based subsystems (LLM providers, MCP transports, web search, etc.).
//
// Why a singleton?
//   `reqwest::Client` is designed to be shared — it pools TCP connections,
//   reuses TLS sessions, and amortizes DNS lookups.  Creating one per
//   provider wastes all of that.  The `.clone()` is just an Arc bump.
//
// Configuration:
//   All HTTP policy lives here: User-Agent, timeouts, TLS/CA settings.
//   Centralizing it means every outbound request gets consistent behavior
//   and a single place to add proxy support, custom CAs, or retry policy.
// ===========================================================================

use std::sync::LazyLock;
use std::time::Duration;

use reqwest::redirect::{Action, Attempt, Policy};

/// User-Agent header sent on every outbound request.
const USER_AGENT: &str = concat!(
    "Dyson/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/joncooperworks/dyson)"
);

/// Default connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default overall request timeout (covers connect + response headers + body).
///
/// Set high because LLM streaming responses can take minutes for long
/// generations.  The connect timeout above catches unreachable hosts fast.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum number of HTTP redirects to follow.  Anthropic/OpenAI/Gemini APIs
/// typically issue zero redirects; public sites rarely chain more than 2–3.
/// reqwest's default of 10 is gratuitous and enlarges the SSRF blast radius
/// if a redirect validator ever misses a hop.
const MAX_REDIRECTS: usize = 4;

/// Ensure the rustls crypto provider is installed (idempotent).
///
/// Called automatically when the HTTP client is first used.  Also called
/// from `main()` for early failure.  Safe to call multiple times — the
/// second call is a no-op.
pub fn ensure_crypto_provider() {
    // install_default returns Err if already installed — that's fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Redirect policy that refuses hops into internal address space.
///
/// Blocks literal-IP redirects targeting loopback, private (RFC1918),
/// link-local, ULA, or unspecified ranges, and well-known cloud metadata
/// hosts.  Hostnames in redirects are allowed through: a domain that
/// resolves (via DNS rebinding) to a private IP still passes this
/// filter.  The redirect hook is synchronous and cannot do its own DNS
/// resolution, so full hostname-based SSRF defence happens at the
/// *initial* URL via [`verify_url_safe`] — callers accepting untrusted
/// URLs (e.g. `web_fetch`) must call it before dispatching the request.
///
/// Also caps the redirect chain length.
pub fn safe_redirect_policy() -> Policy {
    Policy::custom(|attempt: Attempt| -> Action {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        let host = match attempt.url().host_str() {
            Some(h) => h,
            None => return attempt.follow(),
        };
        // Strip optional bracket on IPv6 literals.
        let raw = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = raw.parse::<std::net::IpAddr>() {
            return match ip {
                std::net::IpAddr::V4(v4) if is_private_v4(v4) => {
                    attempt.error("redirect into private IPv4 address blocked")
                }
                std::net::IpAddr::V6(v6) if is_private_v6(v6) => {
                    attempt.error("redirect into private IPv6 address blocked")
                }
                _ => attempt.follow(),
            };
        }
        if is_metadata_host(host) {
            return attempt.error("redirect into cloud metadata host blocked");
        }
        attempt.follow()
    })
}

/// Whether an IPv4 address falls inside a range the HTTP layer refuses
/// to connect to (loopback, RFC1918, link-local, broadcast, multicast,
/// unspecified, or RFC 6598 CGNAT).
///
/// Exposed so every SSRF check in the codebase routes through the same
/// predicate — no second set of rules to drift out of sync with this one.
pub fn is_private_v4(ip: std::net::Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || ip.is_multicast()
        // RFC 6598 shared address space (CGNAT): 100.64.0.0/10
        || (ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 64)
        // Cloud metadata: 169.254.169.254 is already link-local, covered above.
        // Carrier-grade / reserved: 192.0.0.0/24, 192.0.2.0/24, 198.18.0.0/15,
        // 198.51.100.0/24, 203.0.113.0/24 — documentation / benchmarking only.
        || ip.octets()[0] == 0
}

/// Companion to [`is_private_v4`] for IPv6.
pub fn is_private_v6(ip: std::net::Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // Unique local addresses: fc00::/7
        || (ip.segments()[0] & 0xfe00) == 0xfc00
        // Link-local: fe80::/10
        || (ip.segments()[0] & 0xffc0) == 0xfe80
        // IPv4-mapped IPv6 → check the v4 part.
        || matches!(ip.to_ipv4_mapped(), Some(v4) if is_private_v4(v4))
}

/// Whether a hostname matches a well-known cloud metadata service.
pub fn is_metadata_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    matches!(
        h.as_str(),
        "localhost"
            | "metadata.google.internal"
            | "metadata"
            | "metadata.aws.amazon.com"
            | "metadata.azure.com"
            | "metadata.tencentyun.com"
            | "metadata.packet.net"
    )
}

/// Process-wide HTTP client singleton.
static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    ensure_crypto_provider();
    let mut builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(32)
        .redirect(safe_redirect_policy());
    // Wire up HTTP_PROXY / HTTPS_PROXY explicitly because the crate is
    // built with `default-features = false`, which disables reqwest's
    // automatic env-based proxy detection.  The cube image bakes
    // these vars into its env so curl / requests / etc pick them up
    // for free; without this, dyson's own reqwest client would dial
    // every destination directly and silently bypass the host
    // Dyson egress proxy that the cube relies on for upstreams that
    // drop eBPF-SNAT'd connections (Google, GitHub via Microsoft, …).
    //
    // NO_PROXY is honoured separately for each scheme: hosts in
    // NO_PROXY (typically the swarm /llm gateway and the local
    // CoreDNS resolver) bypass the proxy and connect directly.
    if let Some(proxy) = build_proxy_from_env() {
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        // INVARIANT: TLS crypto provider installed above; builder only fails
        // on TLS init, which is fatal (no recovery possible).
        .expect("failed to build HTTP client")
});

/// Read `HTTPS_PROXY` / `HTTP_PROXY` (uppercase or lowercase) from env
/// and turn them into a single `reqwest::Proxy` that handles both
/// schemes, with `NO_PROXY` mapped onto reqwest's exclusion API.
///
/// Returns `None` when no proxy env var is set — the client then
/// connects directly, same as before this function existed.
fn build_proxy_from_env() -> Option<reqwest::Proxy> {
    fn read(name_upper: &str, name_lower: &str) -> Option<String> {
        std::env::var(name_upper)
            .or_else(|_| std::env::var(name_lower))
            .ok()
            .filter(|s| !s.trim().is_empty())
    }
    let https = read("HTTPS_PROXY", "https_proxy");
    let http = read("HTTP_PROXY", "http_proxy");
    let url = https.as_ref().or(http.as_ref())?;
    let proxy = reqwest::Proxy::all(url).ok()?;
    let proxy = if let Some(no) = read("NO_PROXY", "no_proxy") {
        // reqwest expects a comma-separated host list; the env var
        // already uses that convention.
        match reqwest::NoProxy::from_string(&no) {
            Some(np) => proxy.no_proxy(Some(np)),
            None => proxy,
        }
    } else {
        proxy
    };
    Some(proxy)
}

/// Returns the shared HTTP client.
///
/// All outbound HTTP requests should go through this client to get
/// consistent User-Agent, timeouts, and TLS configuration.
pub fn client() -> &'static reqwest::Client {
    &CLIENT
}

/// Verify that every address the URL's hostname resolves to is safe to
/// connect to.  Closes the DNS-rebinding gap in [`safe_redirect_policy`]
/// for callers that accept untrusted URLs: a hostname that resolves to
/// an RFC1918 / loopback / metadata IP is rejected before reqwest ever
/// opens a socket.
///
/// Behavior:
///   - URLs without a host (rare; file://, data:) return an error.
///   - IP-literal hosts are checked directly.
///   - Hostnames are resolved via the OS resolver; every returned
///     address must pass `is_private_v4` / `is_private_v6`.
///
/// A narrow TOCTOU remains: the resolver reqwest uses internally could
/// in principle return a different result than our check.  For real-
/// world DNS rebinding attacks to exploit this, the attacker would need
/// the OS cache to flip between these two resolutions, which in
/// practice requires TTL=0 records plus a carefully timed flip that
/// beats the client's own resolver cache.  This check plus the redirect
/// policy raises the bar significantly over doing nothing.
pub async fn verify_url_safe(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    // Strip bracket on IPv6 literal.
    let raw = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = raw.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) if is_private_v4(v4) => {
                Err(format!("refusing to fetch private IPv4 literal: {v4}"))
            }
            std::net::IpAddr::V6(v6) if is_private_v6(v6) => {
                Err(format!("refusing to fetch private IPv6 literal: {v6}"))
            }
            _ => Ok(()),
        };
    }

    if is_metadata_host(host) {
        return Err(format!("refusing to fetch cloud metadata host: {host}"));
    }

    // Resolve via the OS.  Port is required by lookup_host but irrelevant
    // to the safety check.
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("DNS lookup failed for {host}: {e}"))?;

    let mut saw_any = false;
    for sa in addrs {
        saw_any = true;
        match sa.ip() {
            std::net::IpAddr::V4(v4) if is_private_v4(v4) => {
                return Err(format!(
                    "refusing to fetch {host}: resolves to private IPv4 {v4}"
                ));
            }
            std::net::IpAddr::V6(v6) if is_private_v6(v6) => {
                return Err(format!(
                    "refusing to fetch {host}: resolves to private IPv6 {v6}"
                ));
            }
            _ => {}
        }
    }
    if !saw_any {
        return Err(format!("DNS lookup returned no addresses for {host}"));
    }
    Ok(())
}
