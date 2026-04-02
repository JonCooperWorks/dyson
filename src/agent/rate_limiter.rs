// ===========================================================================
// RateLimiter — per-agent message rate limiting.
//
// Enforces a maximum number of messages within a sliding time window.
// Each Agent instance has its own RateLimiter, so per-agent limiting
// is automatically per-chat for Telegram and per-session for terminal.
//
// Controllers never interact with this directly — it's checked inside
// Agent::run() and is completely invisible to the controller layer.
// ===========================================================================

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::{DysonError, Result};

/// Sliding-window rate limiter.
///
/// Tracks timestamps of recent calls and rejects new ones when the
/// count within `window` exceeds `max_calls`.
pub struct RateLimiter {
    max_calls: usize,
    window: Duration,
    timestamps: Mutex<Vec<Instant>>,
}

impl RateLimiter {
    /// Create a new rate limiter.
    ///
    /// - `max_calls`: maximum number of calls allowed within `window`
    /// - `window`: sliding time window duration
    pub fn new(max_calls: usize, window: Duration) -> Self {
        Self {
            max_calls,
            window,
            timestamps: Mutex::new(Vec::new()),
        }
    }

    /// Check if a new call is allowed.
    ///
    /// Returns `Ok(())` if under the limit, or `Err` if rate limited.
    /// Automatically prunes expired timestamps.
    pub fn check(&self) -> Result<()> {
        let now = Instant::now();
        let mut timestamps = self.timestamps.lock().unwrap();

        // Prune timestamps outside the window.
        timestamps.retain(|&t| now.duration_since(t) < self.window);

        if timestamps.len() >= self.max_calls {
            return Err(DysonError::RateLimit {
                limit: self.max_calls,
                window_secs: self.window.as_secs(),
            });
        }

        timestamps.push(now);
        Ok(())
    }
}
