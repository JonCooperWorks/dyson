//! Mesh addressing primitives.
//!
//! Every peer in the swarm is identified by a [`NodeId`] derived from its
//! Ed25519 public key. Every service hosted on a peer is identified by a
//! [`ServiceName`]. A fully-qualified [`MeshAddr`] is the pair
//! `node_id/service`.
//!
//! Wire form:
//!
//! ```text
//! node_aBc123xYz.../scheduler
//! ```
//!
//! There is no "default service" — every send specifies both halves.
//! This is intentional: it makes routing explicit and keeps logs grep-able.

use serde::{Deserialize, Serialize};

use crate::error::{MeshError, Result};

// ---------------------------------------------------------------------------
// NodeId
// ---------------------------------------------------------------------------

/// A peer's stable identity.
///
/// Derived from the base64url encoding of the peer's 32-byte Ed25519 public
/// key (43 chars, no padding). The encoding is self-authenticating: anyone
/// who sees a `NodeId` can verify a signature claimed by that peer without
/// any external lookup.
///
/// The `NodeId` is opaque from the consumer's perspective. Don't parse it;
/// don't truncate it; don't compose new ones from substrings. Treat it as
/// a black box.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(String);

impl NodeId {
    /// Wrap an already-formatted node id string.
    ///
    /// This does not parse or validate the structure beyond a basic
    /// non-empty check. Prefer constructing a `NodeId` via
    /// [`crate::NodeIdentity::node_id`] which guarantees the right shape.
    pub fn new(s: impl Into<String>) -> Result<Self> {
        let s = s.into();
        if s.is_empty() {
            return Err(MeshError::Address("node_id must not be empty".into()));
        }
        if s.contains('/') {
            return Err(MeshError::Address(
                "node_id must not contain '/' (use MeshAddr for fully-qualified addresses)"
                    .into(),
            ));
        }
        Ok(Self(s))
    }

    /// The textual representation as it appears on the wire.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for NodeId {
    /// Permissive conversion for internal use. Use [`NodeId::new`] when
    /// validating untrusted input.
    fn from(s: String) -> Self {
        Self(s)
    }
}

// ---------------------------------------------------------------------------
// ServiceName
// ---------------------------------------------------------------------------

/// The name of a service hosted on a peer.
///
/// Examples: `"scheduler"`, `"notifier"`, `"mcp"`, `"autoresearch-finetune"`.
///
/// Service names are scoped per-peer. Two different peers can host services
/// with the same name; the receiver demultiplexes by name to deliver each
/// envelope to the right inbox.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ServiceName(String);

impl ServiceName {
    pub fn new(s: impl Into<String>) -> Result<Self> {
        let s = s.into();
        if s.is_empty() {
            return Err(MeshError::Address("service name must not be empty".into()));
        }
        if s.contains('/') {
            return Err(MeshError::Address(
                "service name must not contain '/'".into(),
            ));
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ServiceName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ServiceName {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for ServiceName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

// ---------------------------------------------------------------------------
// MeshAddr
// ---------------------------------------------------------------------------

/// A fully-qualified address: `node_id/service_name`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MeshAddr {
    pub node: NodeId,
    pub service: ServiceName,
}

impl MeshAddr {
    pub fn new(node: NodeId, service: ServiceName) -> Self {
        Self { node, service }
    }

    /// Parse a wire-form address `node_id/service_name`.
    pub fn parse(s: &str) -> Result<Self> {
        let (node, service) = s
            .split_once('/')
            .ok_or_else(|| MeshError::Address(format!("missing '/' in mesh address: {s}")))?;
        Ok(Self {
            node: NodeId::new(node)?,
            service: ServiceName::new(service)?,
        })
    }
}

impl std::fmt::Display for MeshAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.node, self.service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_rejects_empty() {
        assert!(NodeId::new("").is_err());
    }

    #[test]
    fn node_id_rejects_slash() {
        assert!(NodeId::new("ab/cd").is_err());
    }

    #[test]
    fn service_name_rejects_empty() {
        assert!(ServiceName::new("").is_err());
    }

    #[test]
    fn mesh_addr_roundtrip() {
        let a = MeshAddr::parse("nodeABC/scheduler").unwrap();
        assert_eq!(a.node.as_str(), "nodeABC");
        assert_eq!(a.service.as_str(), "scheduler");
        assert_eq!(a.to_string(), "nodeABC/scheduler");
    }

    #[test]
    fn mesh_addr_missing_slash() {
        assert!(MeshAddr::parse("nodeABC").is_err());
    }
}
