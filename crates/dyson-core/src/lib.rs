//! Stable provider-neutral domain contracts shared by every Dyson subsystem.
//!
//! This crate intentionally has no dependency on the agent, controllers,
//! tools, persistence, or provider implementations. Keeping messages and the
//! public error model here prevents those layers from depending on the
//! application composition crate.

pub mod error;
pub mod message;

pub use error::{DysonError, Result};
pub use message::{Artefact, ArtefactKind, ContentBlock, Message, MessageCostMetadata, Role};
pub use message::{estimate_json_tokens, estimate_text_tokens};
