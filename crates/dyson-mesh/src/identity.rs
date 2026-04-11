//! Peer identity: persistent Ed25519 keypairs.
//!
//! Every node in the swarm — workers, the hub, future peers — has its own
//! Ed25519 keypair. The keypair is generated once on first start and
//! persisted to disk at a configurable path (typically `~/.dyson/node.key`)
//! with mode `0600`. Deleting the file is equivalent to "become a new
//! peer": the next start regenerates a fresh keypair with a new
//! [`NodeId`].
//!
//! The [`NodeId`] is the URL-safe base64 encoding of the 32-byte public
//! key (43 chars, no padding). It is *self-authenticating*: any peer that
//! sees a `NodeId` can verify a signature claimed by that peer without
//! consulting an external directory.

use std::fs;
use std::io;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};

use crate::addr::NodeId;
use crate::error::{MeshError, Result};

/// Length of an Ed25519 public key in bytes.
pub const PUBLIC_KEY_LEN: usize = 32;

/// Length of an Ed25519 signature in bytes.
pub const SIGNATURE_LEN: usize = 64;

/// A persistent Ed25519 keypair identifying a peer in the mesh.
///
/// `NodeIdentity` owns the private signing key and exposes signing +
/// verification helpers. The public side is exposed as a [`NodeId`] for
/// addressing.
pub struct NodeIdentity {
    key_pair: Ed25519KeyPair,
    public_bytes: [u8; PUBLIC_KEY_LEN],
    node_id: NodeId,
}

impl std::fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
    }
}

impl NodeIdentity {
    /// Load an identity from disk, or generate a new one if the file is
    /// missing. The keypair is stored as raw PKCS#8 bytes.
    ///
    /// On Unix the resulting file is `chmod 0600`.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            Self::generate(path)
        }
    }

    /// Load an existing identity from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let pkcs8 = fs::read(path)?;
        Self::from_pkcs8(&pkcs8)
    }

    /// Generate a fresh identity and persist it to `path`.
    ///
    /// Refuses to overwrite an existing file.
    pub fn generate(path: &Path) -> Result<Self> {
        if path.exists() {
            return Err(MeshError::Identity(format!(
                "key file already exists at {} — refusing to overwrite",
                path.display()
            )));
        }

        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| MeshError::Identity("Ed25519 key generation failed".into()))?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Atomic write: tmp + chmod + rename.
        let tmp = path.with_extension("key.tmp");
        fs::write(&tmp, pkcs8.as_ref())?;
        set_permissions_0600(&tmp)?;
        fs::rename(&tmp, path)?;

        Self::from_pkcs8(pkcs8.as_ref())
    }

    /// Build an identity from raw PKCS#8 bytes (test / in-memory use).
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self> {
        let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8)
            .map_err(|e| MeshError::Identity(format!("PKCS#8 parse failed: {e}")))?;
        let mut public_bytes = [0u8; PUBLIC_KEY_LEN];
        public_bytes.copy_from_slice(key_pair.public_key().as_ref());

        let node_id = NodeId::new(URL_SAFE_NO_PAD.encode(public_bytes))
            .expect("base64url is not empty");

        Ok(Self {
            key_pair,
            public_bytes,
            node_id,
        })
    }

    /// Generate an in-memory identity for testing. Does not persist.
    pub fn generate_ephemeral() -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .expect("Ed25519 key generation failed");
        Self::from_pkcs8(pkcs8.as_ref()).expect("freshly generated key parses")
    }

    /// The 32-byte Ed25519 public key.
    pub fn public_bytes(&self) -> &[u8; PUBLIC_KEY_LEN] {
        &self.public_bytes
    }

    /// This peer's [`NodeId`].
    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    /// Sign a message, returning the 64-byte detached signature.
    pub fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_LEN] {
        let sig = self.key_pair.sign(message);
        let mut out = [0u8; SIGNATURE_LEN];
        out.copy_from_slice(sig.as_ref());
        out
    }
}

/// Verify an Ed25519 signature against a [`NodeId`]'s embedded public key.
///
/// Decodes the base64url public key from the node id, then runs
/// detached signature verification with `ring`.
pub fn verify_signature(node: &NodeId, message: &[u8], signature: &[u8]) -> Result<()> {
    if signature.len() != SIGNATURE_LEN {
        return Err(MeshError::BadSignature);
    }

    let public_bytes = URL_SAFE_NO_PAD
        .decode(node.as_str())
        .map_err(|_| MeshError::Identity("node_id is not valid base64url".into()))?;

    if public_bytes.len() != PUBLIC_KEY_LEN {
        return Err(MeshError::Identity(format!(
            "node_id decodes to {} bytes (expected {PUBLIC_KEY_LEN})",
            public_bytes.len()
        )));
    }

    let pk = ring::signature::UnparsedPublicKey::new(
        &ring::signature::ED25519,
        &public_bytes,
    );
    pk.verify(message, signature)
        .map_err(|_| MeshError::BadSignature)
}

#[cfg(unix)]
fn set_permissions_0600(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = fs::metadata(path)?.permissions();
    perm.set_mode(0o600);
    fs::set_permissions(path, perm)
}

#[cfg(not(unix))]
fn set_permissions_0600(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.key");

        let first = NodeIdentity::generate(&path).unwrap();
        assert!(path.exists());
        assert_eq!(first.public_bytes().len(), PUBLIC_KEY_LEN);
        assert_eq!(first.node_id().as_str().len(), 43);

        let second = NodeIdentity::load(&path).unwrap();
        assert_eq!(first.public_bytes(), second.public_bytes());
        assert_eq!(first.node_id(), second.node_id());
    }

    #[test]
    fn generate_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.key");

        NodeIdentity::generate(&path).unwrap();
        let err = NodeIdentity::generate(&path).unwrap_err();
        assert!(matches!(err, MeshError::Identity(_)));
    }

    #[test]
    fn load_or_create_creates_then_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.key");

        let first = NodeIdentity::load_or_create(&path).unwrap();
        let second = NodeIdentity::load_or_create(&path).unwrap();
        assert_eq!(first.public_bytes(), second.public_bytes());
    }

    #[test]
    fn sign_verify_roundtrip() {
        let id = NodeIdentity::generate_ephemeral();
        let msg = b"hello mesh";
        let sig = id.sign(msg);
        verify_signature(id.node_id(), msg, &sig).unwrap();
    }

    #[test]
    fn verify_rejects_tampered_message() {
        let id = NodeIdentity::generate_ephemeral();
        let msg = b"hello mesh";
        let sig = id.sign(msg);
        let tampered = b"hello world";
        let err = verify_signature(id.node_id(), tampered, &sig).unwrap_err();
        assert!(matches!(err, MeshError::BadSignature));
    }

    #[test]
    fn verify_rejects_wrong_signer() {
        let id_a = NodeIdentity::generate_ephemeral();
        let id_b = NodeIdentity::generate_ephemeral();
        let msg = b"hello";
        let sig = id_a.sign(msg);
        // Verifying against id_b should fail.
        let err = verify_signature(id_b.node_id(), msg, &sig).unwrap_err();
        assert!(matches!(err, MeshError::BadSignature));
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.key");
        NodeIdentity::generate(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
