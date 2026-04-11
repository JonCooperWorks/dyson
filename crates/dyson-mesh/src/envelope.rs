//! The wire envelope used by the mesh layer.
//!
//! Every message peers exchange is wrapped in a [`MeshEnvelope`]. The
//! envelope carries routing metadata (`from`, `to`), correlation IDs,
//! a TTL, a tagged [`MessageKind`] discriminant for telemetry, and an
//! opaque body the receiving service interprets.
//!
//! ## Format
//!
//! The envelope is JSON-serialized for now. The plan calls for CBOR with a
//! JSON body, but JSON-everywhere keeps the dependency surface tiny and
//! the wire trivially debuggable. Switching the envelope to CBOR is a
//! one-line change behind the [`MeshEnvelope::encode`] /
//! [`MeshEnvelope::decode`] helpers.
//!
//! ## Signing
//!
//! The signature covers the canonical-JSON encoding of the envelope with
//! the `signature` field zeroed. This is independent of the receiving
//! relay's identity stamp — even on a fully-trusted tailnet, signatures
//! give us audit trail and forward-compat with real P2P.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::addr::MeshAddr;
use crate::error::{MeshError, Result};
use crate::identity::{NodeIdentity, SIGNATURE_LEN, verify_signature};

// ---------------------------------------------------------------------------
// RequestId
// ---------------------------------------------------------------------------

/// A request identifier — UUIDv7, sortable and timestamp-prefixed.
///
/// `RequestId` doubles as an idempotency key. Services that need
/// at-most-once semantics deduplicate by recording observed ids in their
/// own state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(pub String);

impl RequestId {
    /// Generate a fresh UUIDv7.
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// MessageKind — typed discriminant for routing + telemetry
// ---------------------------------------------------------------------------

/// The kind of a [`MeshEnvelope`].
///
/// The kind is a typed enum (rather than a free string) so the relay can
/// emit metrics and tracing spans by kind without parsing bodies. New
/// kinds can be added freely; receivers ignore unknown kinds (they just
/// won't be routed to a service that doesn't understand them).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageKind {
    /// Submit a task to a scheduler service.
    SubmitTask,

    /// Hand a task off to a worker for execution.
    TaskAssign,

    /// Worker acknowledges acceptance of a task.
    TaskAccepted,

    /// Worker reports progress on a long-running task.
    TaskProgress,

    /// Worker returns the final result.
    TaskResult,

    /// Cancel an in-flight task.
    TaskCancel,

    /// Worker acknowledges a cancel and reports its outcome.
    TaskCancelAck,

    /// Register a notification channel for a task.
    RegisterNotification,

    /// MCP-over-mesh request from an agent to a service.
    McpCall,

    /// MCP-over-mesh reply from a service back to an agent.
    McpReply,

    /// Escape hatch for skill-defined message types.
    Custom { name: String },
}

// ---------------------------------------------------------------------------
// MeshEnvelope
// ---------------------------------------------------------------------------

/// Default TTL when a service doesn't supply one explicitly.
pub const DEFAULT_TTL: Duration = Duration::from_secs(600);

/// Hard cap on TTLs the relay enforces.
pub const MAX_TTL: Duration = Duration::from_secs(3600);

/// A signed envelope routed across the mesh.
///
/// `signature` is base64url-encoded for JSON-friendliness. The signed
/// bytes are the canonical JSON encoding of the envelope with `signature`
/// set to an empty string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshEnvelope {
    /// Wire format version. Bump when the format changes.
    pub version: u8,

    /// The originating peer + service. Stamped by the sending client; the
    /// relay re-stamps it from the authenticated connection on ingress.
    pub from: MeshAddr,

    /// The destination peer + service.
    pub to: MeshAddr,

    /// Sortable, globally unique request id (UUIDv7).
    pub request_id: RequestId,

    /// If this envelope is a reply, the request id it correlates to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<RequestId>,

    /// Unix-epoch milliseconds at sending time.
    pub ts_ms: u64,

    /// How long the relay should hold this envelope for an offline peer
    /// before dropping it. Capped to [`MAX_TTL`].
    pub ttl_secs: u64,

    /// Typed discriminant for the body.
    pub kind: MessageKind,

    /// The opaque body. Interpreted only by the receiving service.
    pub body: serde_json::Value,

    /// Detached Ed25519 signature, base64url-encoded. Empty when the
    /// envelope is being prepared for signing.
    #[serde(default)]
    pub signature: String,
}

impl MeshEnvelope {
    /// Build an unsigned envelope. Call [`MeshEnvelope::sign`] before
    /// sending.
    pub fn new(
        from: MeshAddr,
        to: MeshAddr,
        kind: MessageKind,
        body: serde_json::Value,
    ) -> Self {
        Self::new_with_ttl(from, to, kind, body, DEFAULT_TTL)
    }

    pub fn new_with_ttl(
        from: MeshAddr,
        to: MeshAddr,
        kind: MessageKind,
        body: serde_json::Value,
        ttl: Duration,
    ) -> Self {
        let ttl = ttl.min(MAX_TTL);
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            version: 1,
            from,
            to,
            request_id: RequestId::new(),
            correlation_id: None,
            ts_ms,
            ttl_secs: ttl.as_secs(),
            kind,
            body,
            signature: String::new(),
        }
    }

    /// Set this envelope as a reply correlated to a previous request.
    pub fn with_correlation(mut self, id: RequestId) -> Self {
        self.correlation_id = Some(id);
        self
    }

    /// Sign the envelope with the supplied identity. The identity's
    /// `node_id` must match `self.from.node` — otherwise the signature
    /// won't verify on the receiving side.
    pub fn sign(&mut self, identity: &NodeIdentity) -> Result<()> {
        if identity.node_id() != &self.from.node {
            return Err(MeshError::Envelope(
                "envelope.from.node does not match signing identity".into(),
            ));
        }
        self.signature.clear();
        let canonical = serde_json::to_vec(&*self)
            .map_err(|e| MeshError::Envelope(format!("serialize: {e}")))?;
        let sig = identity.sign(&canonical);
        self.signature = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            sig,
        );
        Ok(())
    }

    /// Verify the envelope's signature against the embedded `from` peer.
    pub fn verify(&self) -> Result<()> {
        if self.signature.is_empty() {
            return Err(MeshError::Envelope("envelope is unsigned".into()));
        }

        let sig_bytes = base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            &self.signature,
        )
        .map_err(|_| MeshError::Envelope("signature is not base64url".into()))?;

        if sig_bytes.len() != SIGNATURE_LEN {
            return Err(MeshError::BadSignature);
        }

        // Reconstruct the canonical signing form: signature zeroed.
        let mut copy = self.clone();
        copy.signature.clear();
        let canonical = serde_json::to_vec(&copy)
            .map_err(|e| MeshError::Envelope(format!("serialize: {e}")))?;

        verify_signature(&self.from.node, &canonical, &sig_bytes)
    }

    /// Check whether this envelope has expired (now > ts + ttl).
    pub fn is_expired(&self) -> bool {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let ttl_ms = self.ttl_secs.saturating_mul(1000);
        now_ms > self.ts_ms.saturating_add(ttl_ms)
    }

    /// Encode for the wire (JSON bytes).
    pub fn encode(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|e| MeshError::Envelope(e.to_string()))
    }

    /// Decode from wire bytes (JSON).
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(|e| MeshError::Envelope(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::{NodeId, ServiceName};

    fn addr(id: &NodeIdentity, service: &str) -> MeshAddr {
        MeshAddr::new(id.node_id().clone(), ServiceName::from(service))
    }

    #[test]
    fn sign_then_verify() {
        let alice = NodeIdentity::generate_ephemeral();
        let bob = NodeIdentity::generate_ephemeral();

        let mut env = MeshEnvelope::new(
            addr(&alice, "scheduler"),
            addr(&bob, "worker"),
            MessageKind::TaskAssign,
            serde_json::json!({"prompt": "do the thing"}),
        );
        env.sign(&alice).unwrap();
        env.verify().unwrap();
    }

    #[test]
    fn verify_rejects_tampered_body() {
        let alice = NodeIdentity::generate_ephemeral();
        let bob = NodeIdentity::generate_ephemeral();

        let mut env = MeshEnvelope::new(
            addr(&alice, "a"),
            addr(&bob, "b"),
            MessageKind::TaskResult,
            serde_json::json!({"text": "hello"}),
        );
        env.sign(&alice).unwrap();

        env.body = serde_json::json!({"text": "tampered"});
        let err = env.verify().unwrap_err();
        assert!(matches!(err, MeshError::BadSignature));
    }

    #[test]
    fn sign_rejects_mismatched_identity() {
        let alice = NodeIdentity::generate_ephemeral();
        let bob = NodeIdentity::generate_ephemeral();
        let charlie = NodeIdentity::generate_ephemeral();

        let mut env = MeshEnvelope::new(
            addr(&alice, "x"),
            addr(&bob, "y"),
            MessageKind::Custom { name: "noop".into() },
            serde_json::json!({}),
        );
        let err = env.sign(&charlie).unwrap_err();
        assert!(matches!(err, MeshError::Envelope(_)));
    }

    #[test]
    fn ttl_capped_to_max() {
        let alice = NodeIdentity::generate_ephemeral();
        let bob = NodeIdentity::generate_ephemeral();
        let env = MeshEnvelope::new_with_ttl(
            addr(&alice, "x"),
            addr(&bob, "y"),
            MessageKind::TaskProgress,
            serde_json::json!({}),
            Duration::from_secs(99_999),
        );
        assert_eq!(env.ttl_secs, MAX_TTL.as_secs());
    }

    #[test]
    fn encode_decode_roundtrip() {
        let alice = NodeIdentity::generate_ephemeral();
        let bob = NodeIdentity::generate_ephemeral();
        let mut env = MeshEnvelope::new(
            addr(&alice, "a"),
            addr(&bob, "b"),
            MessageKind::SubmitTask,
            serde_json::json!({"prompt": "go"}),
        );
        env.sign(&alice).unwrap();
        let bytes = env.encode().unwrap();
        let decoded = MeshEnvelope::decode(&bytes).unwrap();
        decoded.verify().unwrap();
        assert_eq!(decoded.request_id, env.request_id);
    }

    #[test]
    fn request_id_is_uuidv7_format() {
        let id = RequestId::new();
        // UUID is 36 chars with dashes.
        assert_eq!(id.as_str().len(), 36);
        let parsed = uuid::Uuid::parse_str(id.as_str()).unwrap();
        assert_eq!(parsed.get_version_num(), 7);
    }

    #[test]
    fn correlation_id_optional() {
        let alice = NodeIdentity::generate_ephemeral();
        let bob = NodeIdentity::generate_ephemeral();
        let req_id = RequestId::new();
        let mut env = MeshEnvelope::new(
            addr(&bob, "scheduler"),
            addr(&alice, "agent"),
            MessageKind::McpReply,
            serde_json::json!({}),
        )
        .with_correlation(req_id.clone());
        env.sign(&bob).unwrap();
        env.verify().unwrap();
        assert_eq!(env.correlation_id, Some(req_id));
    }

    #[test]
    fn unsigned_envelope_fails_verify() {
        let alice = NodeIdentity::generate_ephemeral();
        let bob = NodeIdentity::generate_ephemeral();
        let env = MeshEnvelope::new(
            addr(&alice, "a"),
            addr(&bob, "b"),
            MessageKind::TaskCancel,
            serde_json::json!({}),
        );
        let err = env.verify().unwrap_err();
        assert!(matches!(err, MeshError::Envelope(_)));
    }

    #[test]
    fn dummy_node_id_unused_warning_silencer() {
        // Keep NodeId import live in tests.
        let _ = NodeId::new("placeholder").unwrap();
    }
}
