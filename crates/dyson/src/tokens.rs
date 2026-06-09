//! Typed swarm bearer tokens received by dyson.
//!
//! Swarm hands dyson three bearer kinds (see
//! `dyson-swarm/crates/core/src/tokens.rs` for the producing side):
//!
//! - `pt_<32hex>` — chat-provider proxy bearer that authenticates
//!   `dyson` against swarm's `/llm/*` proxy.
//! - `it_<32hex>` — internal-ingest bearer for `POST
//!   /v1/internal/ingest/artefact`.
//! - `st_<32hex>` — state-sync bearer that authorises pushes back
//!   to swarm's durable state surface.
//!
//! All three used to be stored and threaded around as plain `String`,
//! which meant a refactor that swapped a proxy token in where an
//! ingest token was expected only surfaced as a swarm-side 401/404.
//! The newtypes give each kind its own type: the constructor enforces
//! the prefix and body shape, the field type carries the kind, and
//! downstream code is uncompilable when the kinds get crossed.
//!
//! The definitions live in `dyson-common` so dyson and swarm parse the
//! wire identically; re-exporting them here keeps every `crate::tokens::*`
//! call site unchanged. `SessionToken` exists in the shared crate (swarm's
//! browser cookie) but dyson never receives it, so it is not re-exported.

pub use dyson_common::tokens::{
    BadToken, INGEST_TOKEN_PREFIX, IngestToken, PROXY_TOKEN_PREFIX, ProxyToken,
    STATE_SYNC_TOKEN_PREFIX, StateSyncToken, TOKEN_BODY_HEX_LEN,
};

#[cfg(test)]
mod tests {
    use super::*;

    // Parse/round-trip/shape-rejection are exercised by dyson-common's own
    // token tests; these cover only the dyson-specific surface: that the
    // three kinds dyson actually receives are wired to the right prefixes.
    #[test]
    fn dyson_kinds_carry_expected_prefixes() {
        assert_eq!(ProxyToken::PREFIX, "pt_");
        assert_eq!(IngestToken::PREFIX, "it_");
        assert_eq!(StateSyncToken::PREFIX, "st_");
    }

    #[test]
    fn cross_kind_parse_is_rejected() {
        // The whole point of the newtypes: a proxy token must not parse as
        // an ingest token even though both are well-formed.
        let proxy = "pt_0123456789abcdef0123456789abcdef";
        assert_eq!(ProxyToken::parse(proxy).unwrap().as_str(), proxy);
        assert_eq!(IngestToken::parse(proxy), Err(BadToken::WrongPrefix));
    }
}
