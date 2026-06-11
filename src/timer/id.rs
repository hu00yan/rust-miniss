use std::sync::atomic::{AtomicU64, Ordering};

/// A unique identifier for a timer
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(u64);

static TIMER_COUNTER: AtomicU64 = AtomicU64::new(1);

impl Default for TimerId {
    fn default() -> Self {
        Self::new()
    }
}

impl TimerId {
    /// Generates a new unique TimerId
    pub fn new() -> Self {
        TimerId(TIMER_COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}
