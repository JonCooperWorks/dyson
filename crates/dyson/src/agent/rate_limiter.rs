// ===========================================================================
// Rate limiter — priority-aware sliding-window rate limiting.
//
// Two main types:
//
//   RateLimited<T> — the agent's primary access point.  Wraps a value
//     behind a sliding-window rate limiter.  The agent loop calls
//     access() at UserFacing priority.
//
//   RateLimitedHandle<T> — a cloneable handle to the same value and
//     limiter, locked to a specific priority.  Dreams use this to make
//     LLM calls at Background priority through the same rate counter.
//
// Priority levels and their effective capacity (fraction of max_calls):
//
//   UserFacing  — 100%  (interactive agent loop, never throttled early)
//   Background  —  66%  (dreams: memory maintenance, learning synthesis)
//   Scheduled   —  33%  (future: heartbeat/cron tasks, batch operations)
//
// The key invariant: there is no way to reach the LlmClient without
// passing through the rate limiter.  RateLimited owns the Arc<T>, and
// the only way to get a reference is through access() or a handle.
// ===========================================================================

use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::{DysonError, Result};

// ---------------------------------------------------------------------------
// Priority
// ---------------------------------------------------------------------------

/// Priority level for rate-limited access.
///
/// Higher-priority callers can use more of the rate limit window's capacity.
/// Lower-priority callers voluntarily cap themselves to leave headroom for
/// interactive requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// Cron/heartbeat tasks, batch operations.  Uses at most 1/3 of capacity.
    Scheduled = 0,

    /// Dreams and other background cognitive tasks.  Uses at most 2/3 of capacity.
    Background = 1,

    /// Interactive agent loop — user is waiting.  Uses full capacity.
    UserFacing = 2,
}

impl Priority {
    /// Effective capacity as a fraction of `max_calls` for this priority.
    const fn effective_limit(self, max_calls: usize) -> usize {
        match self {
            Self::UserFacing => max_calls,
            Self::Background => max_calls * 2 / 3,
            Self::Scheduled => max_calls / 3,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared rate-limiting state
// ---------------------------------------------------------------------------

/// The shared sliding-window counter.  Wrapped in `Arc` so it can be
/// shared between `RateLimited<T>` and any `RateLimitedHandle<T>`.
struct RateLimiterState {
    max_calls: usize,
    window: Duration,
    timestamps: Mutex<VecDeque<Instant>>,
}

impl RateLimiterState {
    fn check(&self, priority: Priority) -> Result<()> {
        // Fast path for unlimited.
        if self.max_calls == usize::MAX {
            return Ok(());
        }

        let effective_limit = priority.effective_limit(self.max_calls);
        let now = Instant::now();
        let mut timestamps = self.timestamps.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

        // Prune expired timestamps from the front (O(k) where k = expired,
        // instead of O(n) retain over the entire vec).  Timestamps are always
        // pushed in chronological order, so the front is the oldest.
        while let Some(&front) = timestamps.front() {
            if now.duration_since(front) >= self.window {
                timestamps.pop_front();
            } else {
                break;
            }
        }

        if timestamps.len() >= effective_limit {
            return Err(DysonError::RateLimit {
                limit: self.max_calls,
                window_secs: self.window.as_secs(),
            });
        }

        timestamps.push_back(now);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RateLimited<T> — the agent's primary interface
// ---------------------------------------------------------------------------

/// A value of type `T` protected by a sliding-window rate limiter.
///
/// Access to the inner value is gated by [`access()`](RateLimited::access)
/// (for `UserFacing` priority) or [`access_with_priority()`](RateLimited::access_with_priority).
///
/// The inner value is stored in an `Arc` so that [`RateLimitedHandle`]s
/// can share it without cloning the underlying resource.
pub struct RateLimited<T> {
    inner: Arc<T>,
    state: Arc<RateLimiterState>,
}

/// Guard returned by [`RateLimited::access()`].
///
/// Dereferences to `&T`, giving the caller read access to the wrapped
/// value.  The guard is proof that the rate limit was satisfied.
pub struct RateLimitGuard<'a, T> {
    inner: &'a T,
}

impl<'a, T> Deref for RateLimitGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.inner
    }
}

impl<T> RateLimited<T> {
    /// Wrap `value` with a rate limit of `max_calls` per `window`.
    pub fn new(value: T, max_calls: usize, window: Duration) -> Self {
        Self {
            inner: Arc::new(value),
            state: Arc::new(RateLimiterState {
                max_calls,
                window,
                timestamps: Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// Wrap `value` with no rate limit.
    pub fn unlimited(value: T) -> Self {
        Self {
            inner: Arc::new(value),
            state: Arc::new(RateLimiterState {
                max_calls: usize::MAX,
                window: Duration::ZERO,
                timestamps: Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// Attempt to access the inner value at [`Priority::UserFacing`].
    pub fn access(&self) -> Result<RateLimitGuard<'_, T>> {
        self.access_with_priority(Priority::UserFacing)
    }

    /// Attempt to access the inner value at a specific priority.
    pub fn access_with_priority(&self, priority: Priority) -> Result<RateLimitGuard<'_, T>> {
        self.state.check(priority)?;
        Ok(RateLimitGuard { inner: &self.inner })
    }

    /// Create a [`RateLimitedHandle`] at a specific priority.
    ///
    /// The handle shares the same rate counter and the same `Arc<T>`.
    /// This is how dreams get rate-limited access to the LLM client
    /// without being able to bypass the limiter.
    pub fn handle(&self, priority: Priority) -> RateLimitedHandle<T> {
        RateLimitedHandle {
            inner: Arc::clone(&self.inner),
            state: Arc::clone(&self.state),
            priority,
        }
    }

    /// Direct reference to the inner value, bypassing the rate limiter.
    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    /// Consume the wrapper and return the inner value.
    ///
    /// Returns `None` if other handles still hold references.
    pub fn into_inner(self) -> Option<T> {
        Arc::into_inner(self.inner)
    }
}

// ---------------------------------------------------------------------------
// RateLimitedHandle<T> — cloneable, priority-locked handle for background use
// ---------------------------------------------------------------------------

/// A cloneable handle to a rate-limited resource at a fixed priority.
///
/// Created via [`RateLimited::handle()`].  Shares the same rate counter
/// and inner `Arc<T>` as the parent `RateLimited<T>`.
///
/// This is the type dreams receive — they can make LLM calls through it
/// but cannot bypass the rate limiter or change their priority.
pub struct RateLimitedHandle<T> {
    inner: Arc<T>,
    state: Arc<RateLimiterState>,
    priority: Priority,
}

// Manual Clone because Arc<T> doesn't require T: Clone.
impl<T> Clone for RateLimitedHandle<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            state: Arc::clone(&self.state),
            priority: self.priority,
        }
    }
}

impl<T> RateLimitedHandle<T> {
    /// Create a handle with no rate limit at [`Priority::UserFacing`].
    ///
    /// **Test-only.** Production code must obtain handles from a
    /// `ClientRegistry` to ensure all clients are shared and rate-limited.
    ///
    /// Not gated by `#[cfg(test)]` because integration tests in `tests/`
    /// need it.  The `#[doc(hidden)]` keeps it out of public docs, and
    /// `create_client()` being `pub(crate)` prevents external callers
    /// from constructing real clients to pass here.
    #[doc(hidden)]
    pub fn unlimited(value: T) -> Self {
        let rl = RateLimited::unlimited(value);
        rl.handle(Priority::UserFacing)
    }

    /// Attempt to access the inner value at this handle's priority.
    ///
    /// Returns a [`RateLimitGuard`] on success, or `Err(RateLimit)` if
    /// this priority's effective capacity is exhausted.
    pub fn access(&self) -> Result<RateLimitGuard<'_, T>> {
        self.state.check(self.priority)?;
        Ok(RateLimitGuard { inner: &self.inner })
    }

    /// The priority this handle operates at.
    pub const fn priority(&self) -> Priority {
        self.priority
    }

    /// Create a new handle at a different priority, sharing the same
    /// inner value and rate counter.
    ///
    /// This is how dreams get a `Background`-priority handle from the
    /// agent's `UserFacing` handle without needing access to the
    /// original `RateLimited`.
    pub fn with_priority(&self, priority: Priority) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            state: Arc::clone(&self.state),
            priority,
        }
    }

    /// Direct reference to the inner value, bypassing the rate limiter.
    pub fn get_ref(&self) -> &T {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_always_succeeds() {
        let rl = RateLimited::unlimited(42);
        for _ in 0..1000 {
            assert_eq!(*rl.access().unwrap(), 42);
        }
    }

    #[test]
    fn user_facing_uses_full_capacity() {
        let rl = RateLimited::new("val", 10, Duration::from_secs(60));

        for _ in 0..10 {
            assert!(rl.access_with_priority(Priority::UserFacing).is_ok());
        }
        assert!(rl.access_with_priority(Priority::UserFacing).is_err());
    }

    #[test]
    fn background_uses_two_thirds_capacity() {
        let rl = RateLimited::new("val", 9, Duration::from_secs(60));

        // Background gets 9 * 2/3 = 6 slots.
        for _ in 0..6 {
            assert!(rl.access_with_priority(Priority::Background).is_ok());
        }
        // 7th Background call should fail...
        assert!(rl.access_with_priority(Priority::Background).is_err());
        // ...but UserFacing can still use the remaining capacity.
        assert!(rl.access_with_priority(Priority::UserFacing).is_ok());
    }

    #[test]
    fn scheduled_uses_one_third_capacity() {
        let rl = RateLimited::new("val", 9, Duration::from_secs(60));

        // Scheduled gets 9 / 3 = 3 slots.
        for _ in 0..3 {
            assert!(rl.access_with_priority(Priority::Scheduled).is_ok());
        }
        assert!(rl.access_with_priority(Priority::Scheduled).is_err());
        assert!(rl.access_with_priority(Priority::Background).is_ok());
        assert!(rl.access_with_priority(Priority::UserFacing).is_ok());
    }

    #[test]
    fn mixed_priorities_share_window() {
        let rl = RateLimited::new("val", 6, Duration::from_secs(60));

        // Scheduled: 6/3 = 2 slots.
        assert!(rl.access_with_priority(Priority::Scheduled).is_ok());
        assert!(rl.access_with_priority(Priority::Scheduled).is_ok());
        assert!(rl.access_with_priority(Priority::Scheduled).is_err());

        // Background: 6*2/3 = 4 slots total, 2 already used.
        assert!(rl.access_with_priority(Priority::Background).is_ok());
        assert!(rl.access_with_priority(Priority::Background).is_ok());
        assert!(rl.access_with_priority(Priority::Background).is_err());

        // UserFacing: 6 slots total, 4 already used.
        assert!(rl.access_with_priority(Priority::UserFacing).is_ok());
        assert!(rl.access_with_priority(Priority::UserFacing).is_ok());
        assert!(rl.access_with_priority(Priority::UserFacing).is_err());
    }

    #[test]
    fn default_access_is_user_facing() {
        let rl = RateLimited::new("val", 3, Duration::from_secs(60));

        for _ in 0..3 {
            assert!(rl.access().is_ok());
        }
        assert!(rl.access().is_err());
    }

    #[test]
    fn handle_shares_state_with_parent() {
        let rl = RateLimited::new("val", 6, Duration::from_secs(60));
        let handle = rl.handle(Priority::Background);

        // Use 2 slots via the handle.
        assert!(handle.access().is_ok());
        assert!(handle.access().is_ok());

        // The parent sees those 2 slots consumed.
        // UserFacing: 6 total, 2 used → 4 remaining.
        for _ in 0..4 {
            assert!(rl.access().is_ok());
        }
        assert!(rl.access().is_err());
    }

    #[test]
    fn handle_respects_its_priority() {
        let rl = RateLimited::new("val", 9, Duration::from_secs(60));
        let bg_handle = rl.handle(Priority::Background);
        let sched_handle = rl.handle(Priority::Scheduled);

        // Scheduled: 3 slots.
        for _ in 0..3 {
            assert!(sched_handle.access().is_ok());
        }
        assert!(sched_handle.access().is_err());

        // Background: 6 total, 3 used → 3 remaining.
        for _ in 0..3 {
            assert!(bg_handle.access().is_ok());
        }
        assert!(bg_handle.access().is_err());

        // UserFacing: 9 total, 6 used → 3 remaining.
        for _ in 0..3 {
            assert!(rl.access().is_ok());
        }
        assert!(rl.access().is_err());
    }

    #[test]
    fn handle_is_cloneable() {
        let rl = RateLimited::new("val", 6, Duration::from_secs(60));
        let h1 = rl.handle(Priority::Background);
        let h2 = h1.clone();

        // Both clones share the same state.
        assert!(h1.access().is_ok());
        assert!(h2.access().is_ok());
        // Background limit is 4 (6 * 2/3), 2 used → 2 left.
        assert!(h1.access().is_ok());
        assert!(h2.access().is_ok());
        // Now at 4 — exhausted for Background.
        assert!(h1.access().is_err());
        assert!(h2.access().is_err());
    }

    #[test]
    fn get_ref_bypasses_limiter() {
        let rl = RateLimited::new(99, 1, Duration::from_secs(60));
        assert!(rl.access().is_ok());
        assert!(rl.access().is_err());
        assert_eq!(*rl.get_ref(), 99);
    }

    #[test]
    fn handle_get_ref_bypasses_limiter() {
        let rl = RateLimited::new(42, 1, Duration::from_secs(60));
        let handle = rl.handle(Priority::UserFacing);
        assert!(handle.access().is_ok());
        assert!(handle.access().is_err());
        assert_eq!(*handle.get_ref(), 42);
    }

    #[test]
    fn with_priority_shares_state() {
        let rl = RateLimited::new("val", 6, Duration::from_secs(60));
        let uf = rl.handle(Priority::UserFacing);
        let bg = uf.with_priority(Priority::Background);

        // Background: 6 * 2/3 = 4 slots.
        for _ in 0..4 {
            assert!(bg.access().is_ok());
        }
        assert!(bg.access().is_err());

        // UserFacing sees the same 4 consumed → 2 left.
        assert!(uf.access().is_ok());
        assert!(uf.access().is_ok());
        assert!(uf.access().is_err());
    }

    #[test]
    fn unlimited_handle_always_succeeds() {
        let h = RateLimitedHandle::unlimited(77);
        for _ in 0..1000 {
            assert_eq!(*h.access().unwrap(), 77);
        }
    }
}
