// ===========================================================================
// Tool Limiter — per-turn rate limiting and cooldown enforcement.
//
// Prevents runaway tool use by enforcing per-turn call count limits and
// minimum cooldown periods between consecutive calls to the same tool.
// ===========================================================================

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::DysonError;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default maximum calls per tool per turn.
const DEFAULT_PER_TURN_LIMIT: usize = 50;

/// Default cooldown between consecutive calls to the same tool.
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// ToolLimiter
// ---------------------------------------------------------------------------

/// Enforces per-turn call count limits and cooldown periods for tool calls.
pub struct ToolLimiter {
    /// Maximum number of calls allowed per tool per turn.
    per_turn_limit: usize,

    /// Minimum time between consecutive calls to the same tool.
    cooldown: Duration,

    /// Per-tool call counts for the current turn.
    turn_counts: HashMap<String, usize>,

    /// Timestamp of the last call to each tool.
    last_call: HashMap<String, Instant>,
}

impl Default for ToolLimiter {
    fn default() -> Self {
        Self {
            per_turn_limit: DEFAULT_PER_TURN_LIMIT,
            cooldown: DEFAULT_COOLDOWN,
            turn_counts: HashMap::new(),
            last_call: HashMap::new(),
        }
    }
}

impl ToolLimiter {
    /// Create a limiter suitable for batch execution (no cooldown).
    ///
    /// Within a single agent turn, multiple calls to the same tool should
    /// not be gated by cooldown — that's what per-turn limits are for.
    /// Cooldown is useful for rate-limiting across turns or external callers.
    pub fn for_agent() -> Self {
        Self {
            per_turn_limit: DEFAULT_PER_TURN_LIMIT,
            cooldown: Duration::ZERO,
            turn_counts: HashMap::new(),
            last_call: HashMap::new(),
        }
    }

    /// Check whether a tool call is allowed.
    ///
    /// Returns `Ok(())` if the call is within limits, or an error if the
    /// per-turn limit is exceeded or the cooldown period hasn't elapsed.
    ///
    /// This method also records the call — it increments the counter and
    /// updates the last-call timestamp on success.
    pub fn check(&mut self, tool_name: &str) -> crate::Result<()> {
        // Check per-turn limit.
        let count = self.turn_counts.entry(tool_name.to_string()).or_insert(0);
        if *count >= self.per_turn_limit {
            return Err(DysonError::Tool {
                tool: tool_name.to_string(),
                message: format!(
                    "per-turn limit exceeded ({}/{})",
                    *count, self.per_turn_limit
                ),
            });
        }

        // Check cooldown.
        if let Some(last) = self.last_call.get(tool_name) {
            let elapsed = last.elapsed();
            if elapsed < self.cooldown {
                return Err(DysonError::Tool {
                    tool: tool_name.to_string(),
                    message: format!(
                        "cooldown not elapsed ({:?} < {:?})",
                        elapsed, self.cooldown
                    ),
                });
            }
        }

        // Record the call.
        *count += 1;
        self.last_call.insert(tool_name.to_string(), Instant::now());
        Ok(())
    }

    /// Reset per-turn counters (called at the end of each turn).
    ///
    /// Cooldown timestamps are preserved — they span across turns.
    pub fn reset_turn(&mut self) {
        self.turn_counts.clear();
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod test_tool_limiter {
    use super::*;

    #[test]
    fn allows_within_limits() {
        let mut l = ToolLimiter {
            cooldown: Duration::ZERO,
            ..Default::default()
        };
        for _ in 0..50 {
            l.check("bash").unwrap();
        }
    }

    #[test]
    fn blocks_over_per_turn_limit() {
        let mut l = ToolLimiter {
            cooldown: Duration::ZERO,
            ..Default::default()
        };
        for _ in 0..50 {
            l.check("bash").unwrap();
        }
        assert!(l.check("bash").is_err());
    }

    #[test]
    fn resets_on_new_turn() {
        let mut l = ToolLimiter {
            cooldown: Duration::ZERO,
            ..Default::default()
        };
        for _ in 0..50 {
            l.check("bash").unwrap();
        }
        l.reset_turn();
        assert!(l.check("bash").is_ok());
    }

    #[test]
    fn respects_cooldown() {
        let mut l = ToolLimiter::default();
        l.check("bash").unwrap();
        assert!(l.check("bash").is_err());
        std::thread::sleep(std::time::Duration::from_secs(1));
        assert!(l.check("bash").is_ok());
    }
}
