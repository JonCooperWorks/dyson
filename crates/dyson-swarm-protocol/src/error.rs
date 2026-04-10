//! Error types for the swarm protocol crate.
//!
//! This error type is deliberately minimal and self-contained — it does
//! not depend on any Dyson-specific error machinery.  Consumers convert
//! from `ProtocolError` into their own error types via `From` impls.

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("signature verification failed: {0}")]
    Signature(String),

    #[error("invalid public key: {0}")]
    PublicKey(String),

    #[error("invalid wire format: {0}")]
    WireFormat(String),
}

pub type Result<T> = std::result::Result<T, ProtocolError>;
