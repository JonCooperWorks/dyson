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
//! This module gives each kind its own newtype: the constructor
//! enforces the prefix and body shape, the field type carries the
//! kind, and downstream code is uncompilable when the kinds get
//! crossed.
//!
//! The shape mirrors the swarm-side module exactly so the two ends
//! agree at the wire boundary.

use serde::{Deserialize, Serialize};

/// Number of ASCII-hex characters in a token body.
pub const TOKEN_BODY_HEX_LEN: usize = 32;

pub const PROXY_TOKEN_PREFIX: &str = "pt_";
pub const INGEST_TOKEN_PREFIX: &str = "it_";
pub const STATE_SYNC_TOKEN_PREFIX: &str = "st_";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadToken {
    /// Empty or too short for the prefix, or a body shorter than 32 chars.
    TooShort,
    /// Body longer than 32 chars.
    TooLong,
    /// Wrong prefix for the kind being parsed.
    WrongPrefix,
    /// Body has the right length but is not 32 hex chars.
    BadBody,
}

impl std::fmt::Display for BadToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TooShort => "token too short",
            Self::TooLong => "token too long",
            Self::WrongPrefix => "token has wrong kind prefix",
            Self::BadBody => "token body is not 32 hex chars",
        })
    }
}

impl std::error::Error for BadToken {}

fn validate(token: &str, prefix: &str) -> Result<(), BadToken> {
    if token.len() < prefix.len() {
        return Err(BadToken::TooShort);
    }
    let Some(rest) = token.strip_prefix(prefix) else {
        return Err(BadToken::WrongPrefix);
    };
    // Report the length error with explicit direction — an over-length body
    // returning "too short" is a contradictory diagnostic.
    match rest.len().cmp(&TOKEN_BODY_HEX_LEN) {
        std::cmp::Ordering::Less => return Err(BadToken::TooShort),
        std::cmp::Ordering::Greater => return Err(BadToken::TooLong),
        std::cmp::Ordering::Equal => {}
    }
    if !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(BadToken::BadBody);
    }
    Ok(())
}

macro_rules! typed_token {
    ($name:ident, $prefix:ident, $doc:expr) => {
        #[doc = $doc]
        ///
        /// Constructed only via [`Self::parse`] (which enforces the
        /// prefix) or via serde Deserialize (which itself goes
        /// through `parse`).
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(pub(crate) String);

        impl $name {
            pub fn parse(s: impl Into<String>) -> Result<Self, BadToken> {
                let s = s.into();
                validate(&s, $prefix)?;
                Ok(Self(s))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_inner(self) -> String {
                self.0
            }

            pub const PREFIX: &'static str = $prefix;
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        // serde Deserialize that re-validates the prefix + body.
        // Receiving a bad-shape token from swarm collapses to a
        // deserialize error at the boundary rather than threading a
        // malformed value into downstream code.
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let raw = String::deserialize(deserializer)?;
                Self::parse(raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

typed_token!(
    ProxyToken,
    PROXY_TOKEN_PREFIX,
    "Chat-provider proxy bearer.  dyson sends it on every call to swarm's `/llm/*` proxy."
);
typed_token!(
    IngestToken,
    INGEST_TOKEN_PREFIX,
    "Internal-ingest bearer.  dyson sends it on `POST /v1/internal/ingest/artefact` to swarm."
);
typed_token!(
    StateSyncToken,
    STATE_SYNC_TOKEN_PREFIX,
    "State-sync bearer.  Authorises pushes to swarm's durable state surface."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_well_formed_proxy_token() {
        let s = "pt_0123456789abcdef0123456789abcdef";
        assert_eq!(ProxyToken::parse(s).unwrap().as_str(), s);
        assert_eq!(ProxyToken::PREFIX, "pt_");
    }

    #[test]
    fn parse_rejects_wrong_prefix() {
        assert_eq!(
            ProxyToken::parse("it_0123456789abcdef0123456789abcdef"),
            Err(BadToken::WrongPrefix)
        );
    }

    #[test]
    fn parse_rejects_bad_body() {
        assert_eq!(
            ProxyToken::parse("pt_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"),
            Err(BadToken::BadBody)
        );
    }

    #[test]
    fn ingest_and_state_sync_round_trip() {
        let i = IngestToken::parse("it_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let s = StateSyncToken::parse("st_ffffffffffffffffffffffffffffffff").unwrap();
        assert_eq!(IngestToken::PREFIX, "it_");
        assert_eq!(StateSyncToken::PREFIX, "st_");
        assert_eq!(i.as_str().len(), 35);
        assert_eq!(s.as_str().len(), 35);
    }

    #[test]
    fn serde_round_trips_typed_token_through_json() {
        let original = ProxyToken::parse("pt_0123456789abcdef0123456789abcdef").unwrap();
        let json = serde_json::to_string(&original).unwrap();
        // serialised as a plain JSON string thanks to #[serde(transparent)]
        assert_eq!(json, "\"pt_0123456789abcdef0123456789abcdef\"");
        let round: ProxyToken = serde_json::from_str(&json).unwrap();
        assert_eq!(round, original);
    }

    #[test]
    fn serde_rejects_malformed_input() {
        let bad: Result<ProxyToken, _> = serde_json::from_str("\"pt_notenoughhex\"");
        assert!(bad.is_err(), "deserialize must fail on bad token shape");
    }

    #[test]
    fn too_short_input_is_rejected() {
        assert_eq!(ProxyToken::parse("pt_short"), Err(BadToken::TooShort));
        assert_eq!(ProxyToken::parse(""), Err(BadToken::TooShort));
    }

    #[test]
    fn over_length_body_is_too_long_not_too_short() {
        // A 33-hex body must not report "too short".
        let s = format!("pt_{}", "a".repeat(TOKEN_BODY_HEX_LEN + 1));
        assert_eq!(ProxyToken::parse(s), Err(BadToken::TooLong));
    }
}
