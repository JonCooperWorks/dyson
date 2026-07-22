//! Application façade and configured persistence factory.

pub use dyson_persistence::{ChatHistory, DiskChatHistory};

pub(crate) mod coalesce {
    pub(crate) use dyson_persistence::CoalescingPersister;
}

use crate::config::ChatHistoryConfig;
use crate::error::{DysonError, Result};

/// Create the configured per-chat persistence backend.
pub fn create_chat_history(config: &ChatHistoryConfig) -> Result<Box<dyn ChatHistory>> {
    match config.backend.as_str() {
        "disk" => {
            let store =
                DiskChatHistory::new_from_connection_string(config.connection_string.expose())?;
            Ok(Box::new(store))
        }
        other => Err(DysonError::Config(format!(
            "unknown chat_history backend: '{other}'.  Supported: 'disk'."
        ))),
    }
}
