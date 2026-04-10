//! Hub keypair management and task signing.
//!
//! The hub has a single Ed25519 keypair.  On first run, it is generated
//! and persisted to `data_dir/hub.key` (PKCS#8, 0600).  On subsequent
//! runs, it is loaded from the same file.
//!
//! The wire format for signed tasks is:
//!
//! ```text
//! ┌──────────┬──────────────┬──────────────────────────┐
//! │ version  │  signature   │  canonical JSON payload  │
//! │ 1 byte   │  64 bytes    │  N bytes                 │
//! └──────────┴──────────────┴──────────────────────────┘
//! ```
//!
//! This MUST match `dyson_swarm_protocol::verify::verify_signed_payload`
//! byte-for-byte — the unit test at the bottom of this module does a
//! sign-then-verify roundtrip against the protocol crate's verifier to
//! catch any drift.

use std::fs;
use std::io;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use thiserror::Error;

const V1: u8 = 0x01;
const ED25519_SIGNATURE_LEN: usize = 64;

/// Errors that can occur while loading or generating a hub keypair.
#[derive(Debug, Error)]
pub enum KeyError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("failed to generate Ed25519 keypair")]
    Generate,
    #[error("failed to parse PKCS#8 keypair at {path}: {message}")]
    Parse { path: String, message: String },
}

/// An in-memory Ed25519 keypair the hub uses to sign tasks.
pub struct HubKeyPair {
    key_pair: Ed25519KeyPair,
    public_bytes: [u8; 32],
}

impl HubKeyPair {
    /// Load from `path`, generating and persisting a new PKCS#8 keypair
    /// on first run.
    ///
    /// On generation the file is created with mode `0600` on Unix.  On
    /// subsequent runs the existing file is loaded silently.
    pub fn load_or_generate(path: &Path) -> Result<Self, KeyError> {
        if path.exists() {
            let pkcs8 = fs::read(path)?;
            let key_pair = Ed25519KeyPair::from_pkcs8(&pkcs8).map_err(|e| KeyError::Parse {
                path: path.display().to_string(),
                message: e.to_string(),
            })?;
            let mut public_bytes = [0u8; 32];
            public_bytes.copy_from_slice(key_pair.public_key().as_ref());
            return Ok(Self {
                key_pair,
                public_bytes,
            });
        }

        // Generate.
        let rng = SystemRandom::new();
        let pkcs8 =
            Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| KeyError::Generate)?;
        let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|e| {
            KeyError::Parse {
                path: path.display().to_string(),
                message: e.to_string(),
            }
        })?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write atomically (write-then-rename).
        let tmp = path.with_extension("key.tmp");
        fs::write(&tmp, pkcs8.as_ref())?;
        set_permissions_0600(&tmp)?;
        fs::rename(&tmp, path)?;

        let mut public_bytes = [0u8; 32];
        public_bytes.copy_from_slice(key_pair.public_key().as_ref());

        Ok(Self {
            key_pair,
            public_bytes,
        })
    }

    /// The 32-byte Ed25519 public key.
    pub fn public_bytes(&self) -> &[u8; 32] {
        &self.public_bytes
    }

    /// Return the config-format public key string: `"v1:<base64>"`.
    ///
    /// This is what node operators paste into `dyson.json`.
    pub fn public_key_config(&self) -> String {
        format!("v1:{}", STANDARD.encode(self.public_bytes))
    }

    /// Sign a canonical JSON payload, producing the V1 wire bytes:
    /// `version (1) || signature (64) || payload`.
    pub fn sign_task(&self, canonical_json: &[u8]) -> Vec<u8> {
        let sig = self.key_pair.sign(canonical_json);
        let mut wire = Vec::with_capacity(1 + ED25519_SIGNATURE_LEN + canonical_json.len());
        wire.push(V1);
        wire.extend_from_slice(sig.as_ref());
        wire.extend_from_slice(canonical_json);
        wire
    }
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
    use dyson_swarm_protocol::types::SwarmTask;
    use dyson_swarm_protocol::verify::{SwarmPublicKey, verify_signed_payload};

    #[test]
    fn sign_roundtrip_verifies_with_protocol_crate() {
        let dir = tempfile::tempdir().unwrap();
        let key = HubKeyPair::load_or_generate(&dir.path().join("hub.key")).unwrap();

        let task = SwarmTask {
            task_id: "test-task-42".into(),
            prompt: "summarise the release notes".into(),
            payloads: vec![],
            timeout_secs: Some(60),
        };

        let canonical = serde_json::to_vec(&task).unwrap();
        let wire = key.sign_task(&canonical);

        let pk = SwarmPublicKey::from_config(&key.public_key_config()).unwrap();
        let payload = verify_signed_payload(&wire, &pk).unwrap();

        assert_eq!(payload, canonical.as_slice());
        let parsed: SwarmTask = serde_json::from_slice(payload).unwrap();
        assert_eq!(parsed.task_id, "test-task-42");
    }

    #[test]
    fn load_or_generate_is_persistent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.key");

        let first = HubKeyPair::load_or_generate(&path).unwrap();
        assert!(path.exists(), "key file should be created");

        let second = HubKeyPair::load_or_generate(&path).unwrap();
        assert_eq!(first.public_bytes(), second.public_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.key");
        let _ = HubKeyPair::load_or_generate(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key file should be chmod 0600");
    }

    #[test]
    fn public_key_config_has_v1_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let key = HubKeyPair::load_or_generate(&dir.path().join("hub.key")).unwrap();
        let cfg = key.public_key_config();
        assert!(cfg.starts_with("v1:"), "got '{cfg}'");
        // Decodable as base64 and 32 bytes.
        let decoded = STANDARD.decode(cfg.strip_prefix("v1:").unwrap()).unwrap();
        assert_eq!(decoded.len(), 32);
    }
}
