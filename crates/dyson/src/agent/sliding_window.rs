//! Small unkeyed sliding-window counter used by agent-side limiters.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct SlidingWindow {
    max_events: usize,
    window: Duration,
    events: Mutex<VecDeque<Instant>>,
}

impl SlidingWindow {
    pub fn new(max_events: usize, window: Duration) -> Self {
        Self {
            max_events,
            window,
            events: Mutex::new(VecDeque::new()),
        }
    }

    pub fn unlimited() -> Self {
        Self::new(usize::MAX, Duration::ZERO)
    }

    pub fn observe(&self, effective_limit: usize) -> bool {
        if self.max_events == usize::MAX {
            return true;
        }
        let now = Instant::now();
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while let Some(&front) = events.front() {
            if now.duration_since(front) >= self.window {
                events.pop_front();
            } else {
                break;
            }
        }
        if events.len() >= effective_limit {
            return false;
        }
        events.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_over_effective_limit() {
        let window = SlidingWindow::new(10, Duration::from_secs(60));
        assert!(window.observe(1));
        assert!(!window.observe(1));
    }

    #[test]
    fn unlimited_always_allows() {
        let window = SlidingWindow::unlimited();
        for _ in 0..1000 {
            assert!(window.observe(0));
        }
    }
}
