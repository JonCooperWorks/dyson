//! Hub runtime configuration.
//!
//! Parsed from CLI flags in `main.rs`.  The whole struct is passed down
//! into the server setup function; tests construct it directly with
//! `SwarmConfig::for_test`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Runtime configuration for a hub instance.
#[derive(Debug, Clone)]
pub struct SwarmConfig {
    /// Address to bind the HTTP server to.
    pub bind: SocketAddr,
    /// Directory where `hub.key` and `blobs/` live.
    pub data_dir: PathBuf,
    /// Heartbeat timeout — nodes that haven't pinged in this long get reaped.
    pub heartbeat_timeout: Duration,
}

impl SwarmConfig {
    /// Path of the hub's signing key inside the data directory.
    pub fn key_path(&self) -> PathBuf {
        self.data_dir.join("hub.key")
    }
}
