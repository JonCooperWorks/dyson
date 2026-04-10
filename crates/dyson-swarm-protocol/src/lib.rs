//! Wire protocol types and signature verification for the Dyson swarm.
//!
//! This crate is the shared source of truth for the swarm wire format.
//! Both the Dyson agent (node side) and the swarm hub (server side)
//! depend on it for type definitions, serialization, and Ed25519
//! signature verification.
//!
//! Keep this crate small and dependency-light — it is intended to be
//! a stable, portable contract between the hub and its nodes.

pub mod error;
pub mod types;
pub mod verify;

pub use error::{ProtocolError, Result};
