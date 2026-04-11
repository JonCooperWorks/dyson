//! Error type for the mesh primitives.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MeshError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid address: {0}")]
    Address(String),

    #[error("invalid identity: {0}")]
    Identity(String),

    #[error("invalid envelope: {0}")]
    Envelope(String),

    #[error("signature verification failed")]
    BadSignature,

    #[error("unknown peer: {0}")]
    UnknownPeer(String),

    #[error("peer disconnected: {0}")]
    PeerDisconnected(String),

    #[error("envelope expired (ttl exceeded)")]
    Expired,

    #[error("relay shut down")]
    Shutdown,

    #[error("transport: {0}")]
    Transport(String),
}

pub type Result<T> = std::result::Result<T, MeshError>;
