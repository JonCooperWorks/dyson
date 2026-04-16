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
/// hosts.  Hostnames are allowed through: a domain that resolves (via DNS
/// rebinding) to a private IP will still pass this filter — callers that
/// need full SSRF defense must re-resolve the final URL themselves.  This
/// is the best we can do in reqwest's synchronous redirect hook without
/// its own resolver.
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

fn is_private_v4(ip: std::net::Ipv4Addr) -> bool {
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

fn is_private_v6(ip: std::net::Ipv6Addr) -> bool {
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

fn is_metadata_host(host: &str) -> bool {
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
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(32)
        .redirect(safe_redirect_policy())
        .build()
        // INVARIANT: TLS crypto provider installed above; builder only fails
        // on TLS init, which is fatal (no recovery possible).
        .expect("failed to build HTTP client")
});


/// Returns the shared HTTP client.
///
/// All outbound HTTP requests should go through this client to get
/// consistent User-Agent, timeouts, and TLS configuration.
pub fn client() -> &'static reqwest::Client {
    &CLIENT
}
