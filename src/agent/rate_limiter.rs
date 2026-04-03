// ===========================================================================
// RateLimited<T> — generic rate-limiting resource wrapper.
//
// Wraps a value of type T behind a sliding-window rate limiter.  Callers
// obtain access to the inner value through `access()`, which returns a
// guard (similar to `MutexGuard`) only when the rate limit allows it.
//
// This replaces the old `RateLimiter` which required callers to explicitly
// call `check()` before proceeding.
// ===========================================================================

use std::ops::Deref;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::{DysonError, Result};

/// A value of type `T` protected by a sliding-window rate limiter.
///
/// Access to the inner value is gated by [`access()`](RateLimited::access),
/// which enforces a maximum number of calls within a time window — similar
/// to how [`Mutex::lock()`] gates access behind a lock.
pub struct RateLimited<T> {
    inner: T,
    max_calls: usize,
    window: Duration,
    timestamps: Mutex<Vec<Instant>>,
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
            inner: value,
            max_calls,
            window,
            timestamps: Mutex::new(Vec::new()),
        }
    }

    /// Wrap `value` with no rate limit.  [`access()`](Self::access) will
    /// always succeed without tracking timestamps.
    pub fn unlimited(value: T) -> Self {
        Self {
            inner: value,
            max_calls: usize::MAX,
            window: Duration::ZERO,
            timestamps: Mutex::new(Vec::new()),
        }
    }

    /// Attempt to access the inner value.
    ///
    /// Returns a [`RateLimitGuard`] that dereferences to `&T` if the rate
    /// limit allows it, or `Err(DysonError::RateLimit)` if exceeded.
    ///
    /// Uses a sliding-window algorithm: timestamps older than the window
    /// are pruned, and a new timestamp is recorded on success.
    pub fn access(&self) -> Result<RateLimitGuard<'_, T>> {
        // Fast path for unlimited wrappers.
        if self.max_calls == usize::MAX {
            return Ok(RateLimitGuard { inner: &self.inner });
        }

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
        Ok(RateLimitGuard { inner: &self.inner })
    }

    /// Direct reference to the inner value, bypassing the rate limiter.
    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    /// Consume the wrapper and return the inner value.
    pub fn into_inner(self) -> T {
        self.inner
    }
}
