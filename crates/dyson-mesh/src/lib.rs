//! # dyson-mesh
//!
//! Mesh primitives for the Dyson swarm. This crate defines the abstraction
//! that lets nodes talk to each other without caring about the physical
//! topology underneath. The current swarm runs as hub-and-spoke (one
//! coordinator, every peer connects to it), and this crate models that as
//! **mesh semantics over a relay**: peers address each other by
//! `NodeId/ServiceName`, exchange signed envelopes, and never refer to
//! the relay by name.
//!
//! ## What lives in this crate
//!
//! - [`NodeIdentity`] — Ed25519 keypair persisted to disk; the basis of
//!   peer identity
//! - [`NodeId`], [`ServiceName`], [`MeshAddr`] — addressing primitives
//! - [`MeshEnvelope`], [`MessageKind`] — the wire envelope, end-to-end
//!   signed
//! - [`RequestId`] — UUIDv7 (sortable, timestamp-prefixed)
//! - [`MeshClient`] — the trait every transport implements
//! - [`InProcMeshClient`] — in-memory channels, used by tests and by
//!   hub-local services that share a process with the relay
//!
//! ## What does NOT live here
//!
//! No HTTP, no SSE, no relay implementation, no scheduler. Those live in
//! the `swarm` crate. This crate is intentionally tiny and dependency-
//! light so it can be a stable contract between binaries.
//!
//! ## Honest framing
//!
//! There is no real peer-to-peer mesh today. The trait is the abstraction;
//! the only deployed topology is hub-and-spoke. A future `GossipMeshClient`
//! impl can drop in behind this trait without changing the scheduler,
//! notifier, or worker code.

pub mod addr;
pub mod envelope;
pub mod error;
pub mod identity;
pub mod inproc;
pub mod mesh;
pub mod mailbox;

pub use addr::{MeshAddr, NodeId, ServiceName};
pub use envelope::{MessageKind, MeshEnvelope, RequestId};
pub use error::{MeshError, Result};
pub use identity::NodeIdentity;
pub use inproc::{InProcMeshClient, InProcRelay};
pub use mesh::{MeshClient, PeerInfo, PeerEvent, ServiceDescriptor};
