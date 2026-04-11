//! Hub-side peer identity wiring.
//!
//! Re-exports [`dyson_mesh::NodeIdentity`] and provides a hub-flavoured
//! load helper that mirrors the existing `HubKeyPair::load` ergonomics
//! (descriptive errors, default file path).
//!
//! The hub keeps its existing `HubKeyPair` (used to sign legacy
//! `SwarmTask` payloads) for backwards compatibility, and *additionally*
//! has a `NodeIdentity` that is the basis of its peer identity in the
//! mesh layer. The two will eventually merge — for now they coexist so
//! the legacy task path keeps working byte-for-byte while the new mesh
//! layer comes online.

use std::path::Path;

pub use dyson_mesh::NodeIdentity;

/// Load (or create) the hub's mesh identity at `path`.
pub fn load_or_create_hub_identity(path: &Path) -> std::io::Result<NodeIdentity> {
    NodeIdentity::load_or_create(path).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}
