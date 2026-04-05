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

/// Process-wide HTTP client singleton.
static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("failed to build HTTP client")
});

/// Returns the shared HTTP client.
///
/// All outbound HTTP requests should go through this client to get
/// consistent User-Agent, timeouts, and TLS configuration.
pub fn client() -> &'static reqwest::Client {
    &CLIENT
}
