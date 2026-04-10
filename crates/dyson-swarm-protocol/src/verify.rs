// ===========================================================================
// Signature verification — V1 Ed25519, no algorithmic agility.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Verifies signed task envelopes from the swarm hub.  The hub signs
//   tasks with its Ed25519 private key; nodes verify with the public
//   key from their config.
//
// No algorithmic agility:
//   Each version specifies exactly one algorithm.  To change the
//   algorithm, bump the version.  No negotiation, no fallback,
//   no "try both."  This eliminates downgrade attacks and keeps
//   the implementation simple.
//
//   V1 = Ed25519 (RFC 8032) via ring.
//
// Wire format:
//
//   ┌──────────┬───────────────┬──────────────────────────┐
//   │ version  │  signature    │  canonical JSON payload   │
//   │ 1 byte   │  64 bytes     │  N bytes                  │
//   └──────────┴───────────────┴──────────────────────────┘
//
//   The signature is computed over the JSON payload bytes only.
//   The version byte selects which verify function to call.
//
// Public key config format:
//
//   "v1:base64encodedkey..."
//
//   The "v1:" prefix tells Dyson which algorithm the key is for.
//   The rest is the 32-byte Ed25519 public key, base64-encoded.
// ===========================================================================

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ring::signature;

use crate::error::{ProtocolError, Result};

/// The version byte for Ed25519 signatures.
const V1: u8 = 0x01;

/// Size of an Ed25519 signature in bytes.
const ED25519_SIGNATURE_LEN: usize = 64;

/// Size of an Ed25519 public key in bytes.
const ED25519_PUBLIC_KEY_LEN: usize = 32;

/// Minimum wire message size: version (1) + signature (64) + at least 1 byte of payload.
const MIN_WIRE_LEN: usize = 1 + ED25519_SIGNATURE_LEN + 1;

// ---------------------------------------------------------------------------
// SwarmPublicKey
// ---------------------------------------------------------------------------

/// A versioned public key for verifying swarm task signatures.
///
/// Parsed from the config string format: `"v1:base64..."`.
#[derive(Debug, Clone)]
pub struct SwarmPublicKey {
    /// The version this key is for (determines the algorithm).
    version: u8,
    /// The raw public key bytes.
    key_bytes: Vec<u8>,
}

impl SwarmPublicKey {
    /// Parse a public key from the config format: `"v1:base64..."`.
    ///
    /// Returns an error if the version prefix is unknown or the base64
    /// is invalid or the key length is wrong for the algorithm.
    pub fn from_config(s: &str) -> Result<Self> {
        let (version_str, key_b64) = s.split_once(':').ok_or_else(|| {
            ProtocolError::WireFormat("swarm public_key must be in format 'v1:base64...'".into())
        })?;

        let version = match version_str {
            "v1" => V1,
            other => {
                return Err(ProtocolError::PublicKey(format!(
                    "unsupported swarm public key version '{other}' (supported: v1)"
                )));
            }
        };

        let key_bytes = STANDARD.decode(key_b64).map_err(|e| {
            ProtocolError::PublicKey(format!("swarm public_key base64 decode failed: {e}"))
        })?;

        // V1 = Ed25519, key must be exactly 32 bytes.
        if version == V1 && key_bytes.len() != ED25519_PUBLIC_KEY_LEN {
            return Err(ProtocolError::PublicKey(format!(
                "swarm public_key v1 must be 32 bytes, got {}",
                key_bytes.len()
            )));
        }

        Ok(Self {
            version,
            key_bytes,
        })
    }

    /// The version of this key.
    pub fn version(&self) -> u8 {
        self.version
    }
}

// ---------------------------------------------------------------------------
// verify_task — verify a signed wire message and extract the payload
// ---------------------------------------------------------------------------

/// Verify a signed wire message and return the JSON payload bytes.
///
/// The wire format is: `version (1) || signature (64) || payload (N)`.
/// The signature is verified over the payload bytes using the public key.
///
/// Returns the raw payload bytes (canonical JSON) on success.
/// The caller is responsible for deserializing into `SwarmTask`.
pub fn verify_signed_payload<'a>(
    wire_bytes: &'a [u8],
    public_key: &SwarmPublicKey,
) -> Result<&'a [u8]> {
    if wire_bytes.len() < MIN_WIRE_LEN {
        return Err(ProtocolError::WireFormat(format!(
            "signed message too short: {} bytes (minimum {MIN_WIRE_LEN})",
            wire_bytes.len()
        )));
    }

    let version = wire_bytes[0];

    // Version must match the configured key.  No fallback.
    if version != public_key.version {
        return Err(ProtocolError::Signature(format!(
            "signature version mismatch: got {version:#04x}, expected {:#04x}",
            public_key.version
        )));
    }

    let sig_bytes = &wire_bytes[1..1 + ED25519_SIGNATURE_LEN];
    let payload = &wire_bytes[1 + ED25519_SIGNATURE_LEN..];

    match version {
        V1 => verify_v1(payload, sig_bytes, &public_key.key_bytes),
        _ => Err(ProtocolError::Signature(format!(
            "unsupported signature version {version:#04x}"
        ))),
    }?;

    Ok(payload)
}

/// V1 verification: Ed25519 via ring.
fn verify_v1(payload: &[u8], sig_bytes: &[u8], key_bytes: &[u8]) -> Result<()> {
    let public_key =
        signature::UnparsedPublicKey::new(&signature::ED25519, key_bytes);

    public_key
        .verify(payload, sig_bytes)
        .map_err(|_| ProtocolError::Signature("Ed25519 signature verification failed".into()))
}

// ---------------------------------------------------------------------------
// sign (for tests only) — create a signed wire message
// ---------------------------------------------------------------------------

#[cfg(test)]
fn sign_payload(payload: &[u8], key_pair: &signature::Ed25519KeyPair) -> Vec<u8> {
    let sig = key_pair.sign(payload);
    let mut wire = Vec::with_capacity(1 + ED25519_SIGNATURE_LEN + payload.len());
    wire.push(V1);
    wire.extend_from_slice(sig.as_ref());
    wire.extend_from_slice(payload);
    wire
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::KeyPair;

    /// Generate a throwaway Ed25519 keypair for testing.
    fn test_keypair() -> (signature::Ed25519KeyPair, SwarmPublicKey) {
        let rng = SystemRandom::new();
        let pkcs8 = signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let key_pair = signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();

        let pub_bytes = key_pair.public_key().as_ref().to_vec();
        let config_str = format!("v1:{}", STANDARD.encode(&pub_bytes));
        let public_key = SwarmPublicKey::from_config(&config_str).unwrap();

        (key_pair, public_key)
    }

    #[test]
    fn parse_valid_public_key() {
        let (kp, pk) = test_keypair();
        assert_eq!(pk.version, V1);
        assert_eq!(pk.key_bytes, kp.public_key().as_ref());
    }

    #[test]
    fn parse_invalid_version() {
        let err = SwarmPublicKey::from_config("v99:AAAA").unwrap_err();
        assert!(err.to_string().contains("unsupported"));
    }

    #[test]
    fn parse_missing_colon() {
        let err = SwarmPublicKey::from_config("v1base64stuff").unwrap_err();
        assert!(err.to_string().contains("format"));
    }

    #[test]
    fn parse_wrong_key_length() {
        let short_key = STANDARD.encode([0u8; 16]);
        let err = SwarmPublicKey::from_config(&format!("v1:{short_key}")).unwrap_err();
        assert!(err.to_string().contains("32 bytes"));
    }

    #[test]
    fn parse_invalid_base64() {
        let err = SwarmPublicKey::from_config("v1:not-valid-base64!!!").unwrap_err();
        assert!(err.to_string().contains("base64"));
    }

    #[test]
    fn verify_valid_signature() {
        let (kp, pk) = test_keypair();
        let payload = b"{\"task_id\":\"test\",\"prompt\":\"hello\"}";
        let wire = sign_payload(payload, &kp);

        let result = verify_signed_payload(&wire, &pk).unwrap();
        assert_eq!(result, payload);
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        let (kp, pk) = test_keypair();
        let payload = b"{\"task_id\":\"test\",\"prompt\":\"hello\"}";
        let mut wire = sign_payload(payload, &kp);

        // Tamper with the last byte of the payload.
        let last = wire.len() - 1;
        wire[last] ^= 0xFF;

        let err = verify_signed_payload(&wire, &pk).unwrap_err();
        assert!(err.to_string().contains("verification failed"));
    }

    #[test]
    fn verify_rejects_tampered_signature() {
        let (kp, pk) = test_keypair();
        let payload = b"{\"task_id\":\"test\",\"prompt\":\"hello\"}";
        let mut wire = sign_payload(payload, &kp);

        // Tamper with the signature (byte 1).
        wire[1] ^= 0xFF;

        let err = verify_signed_payload(&wire, &pk).unwrap_err();
        assert!(err.to_string().contains("verification failed"));
    }

    #[test]
    fn verify_rejects_wrong_version() {
        let (kp, pk) = test_keypair();
        let payload = b"{\"task_id\":\"test\",\"prompt\":\"hello\"}";
        let mut wire = sign_payload(payload, &kp);

        // Change version byte to 0x02.
        wire[0] = 0x02;

        let err = verify_signed_payload(&wire, &pk).unwrap_err();
        assert!(err.to_string().contains("version mismatch"));
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let (kp, _) = test_keypair();
        let (_, wrong_pk) = test_keypair(); // different keypair

        let payload = b"{\"task_id\":\"test\",\"prompt\":\"hello\"}";
        let wire = sign_payload(payload, &kp);

        let err = verify_signed_payload(&wire, &wrong_pk).unwrap_err();
        assert!(err.to_string().contains("verification failed"));
    }

    #[test]
    fn verify_rejects_too_short() {
        let (_, pk) = test_keypair();

        let err = verify_signed_payload(&[0x01; 10], &pk).unwrap_err();
        assert!(err.to_string().contains("too short"));
    }

    #[test]
    fn verify_rejects_empty() {
        let (_, pk) = test_keypair();

        let err = verify_signed_payload(&[], &pk).unwrap_err();
        assert!(err.to_string().contains("too short"));
    }

    #[test]
    fn roundtrip_sign_verify_json_task() {
        let (kp, pk) = test_keypair();

        let task = crate::types::SwarmTask {
            task_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            prompt: "Fine-tune llama-3".into(),
            payloads: vec![],
            timeout_secs: Some(3600),
        };

        let payload = serde_json::to_vec(&task).unwrap();
        let wire = sign_payload(&payload, &kp);

        let verified = verify_signed_payload(&wire, &pk).unwrap();
        let parsed: crate::types::SwarmTask = serde_json::from_slice(verified).unwrap();

        assert_eq!(parsed.task_id, task.task_id);
        assert_eq!(parsed.prompt, task.prompt);
        assert_eq!(parsed.timeout_secs, Some(3600));
    }
}
