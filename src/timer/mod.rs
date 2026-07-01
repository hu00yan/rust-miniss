use slab::Slab;
use std::task::Waker;
use std::time::{Duration, Instant};

pub mod entry;
pub mod id;
pub mod interval;
pub mod sleep;
pub mod timeout;

pub use entry::Entry;
pub use id::TimerId;
pub use interval::Interval;
pub use sleep::SleepFuture;
pub use timeout::{Timeout, TimeoutError};

/// Sleep for the given duration
pub async fn sleep(duration: Duration) {
    SleepFuture::new(duration).await
}

/// Apply a timeout to any future
///
/// Returns a `Result<T, TimeoutError>` where `T` is the output of the original future.
/// If the future completes before the timeout, the result is `Ok(output)`.
/// If the timeout expires first, the result is `Err(TimeoutError)`.
///
/// # Examples
///
/// ```rust,no_run
/// use rust_miniss::timer;
/// use std::time::Duration;
///
/// # async fn example() {
/// let result = timer::timeout(Duration::from_secs(1), async {
///     timer::sleep(Duration::from_millis(500)).await;
///     "completed"
/// }).await;
///
/// match result {
///     Ok(value) => println!("Completed with: {}", value),
///     Err(_) => println!("Timed out!"),
/// }
/// # }
/// ```
pub async fn timeout<F>(duration: Duration, future: F) -> Result<F::Output, TimeoutError>
where
    F: std::future::Future,
{
    Timeout::new(future, duration).await
}

/// Add timeout combinator to Future trait
pub trait FutureExt: std::future::Future + Sized {
    fn with_timeout(self, duration: Duration) -> Timeout<Self> {
        Timeout::new(self, duration)
    }
}

impl<F: std::future::Future> FutureExt for F {}

/// A high-performance timer wheel implementation for scheduling timeouts.
///
/// Each wheel slot uses a `Slab` so timer cancel stays O(1) without hash lookup.
pub struct TimerWheel {
    slots: Vec<Slab<Entry>>,
    slot_generations: Vec<Vec<u32>>,
    resolution_ms: u64,
    num_slots: usize,
    current_tick: u64,
    start_time: Instant,
    pending_timers: usize,
}

impl Default for TimerWheel {
    fn default() -> Self {
        Self::new(4096, 1)
    }
}

impl TimerWheel {
    /// Creates a new TimerWheel with the specified number of slots and resolution
    pub fn new(num_slots: usize, resolution_ms: u64) -> Self {
        assert!(num_slots > 0, "Timer wheel must have at least one slot");
        assert!(
            resolution_ms > 0,
            "Timer resolution must be greater than zero"
        );
        let mut slots = Vec::with_capacity(num_slots);
        let mut slot_generations = Vec::with_capacity(num_slots);
        for _ in 0..num_slots {
            slots.push(Slab::with_capacity(crate::config::EXPECTED_WAKEUP_COUNT));
            slot_generations.push(Vec::with_capacity(crate::config::EXPECTED_WAKEUP_COUNT));
        }

        Self {
            slots,
            slot_generations,
            resolution_ms,
            num_slots,
            current_tick: 0,
            start_time: Instant::now(),
            pending_timers: 0,
        }
    }

    /// Schedules a timer to expire at the specified time.
    ///
    /// Uses ceiling division for tick computation so that the timer never fires
    /// before `at`. This prevents a subtle bug where a timer fires at tick N
    /// due to floor division, but `Instant::now()` is still before the deadline.
    ///
    /// Returns a TimerId that can be used to cancel the timer.
    pub fn schedule(&mut self, at: Instant, waker: &Waker) -> TimerId {
        debug_assert!(self.resolution_ms > 0, "Timer resolution must be non-zero");

        // Use ceiling division: ensures the deadline tick is strictly at or after `at`.
        // Without this, a timer scheduled at 7.5ms would be placed at tick 7 (floor),
        // fire at real time 7.0ms — 0.5ms before the user's deadline.
        let deadline_tick = self.instant_to_tick_ceil(at).max(self.current_tick);
        let distance = deadline_tick.saturating_sub(self.current_tick);
        let rounds = distance / self.num_slots as u64;
        let slot_index = ((self.current_tick + distance) % self.num_slots as u64) as usize;

        let slot = self
            .slots
            .get_mut(slot_index)
            .expect("slot index must be valid");
        let key = slot.insert(Entry {
            waker: waker.clone(),
            remaining_rounds: rounds,
        });
        let generations = self
            .slot_generations
            .get_mut(slot_index)
            .expect("slot generation index must be valid");
        if key >= generations.len() {
            generations.resize(key + 1, 0);
        }
        let generation = generations[key].wrapping_add(1).max(1);
        generations[key] = generation;
        let timer_id = TimerId::from_parts(slot_index, key, generation);
        self.pending_timers += 1;
        timer_id
    }

    /// Attempts to cancel a timer with the given ID
    ///
    /// Returns true if the timer was found and cancelled, false otherwise
    pub fn cancel(&mut self, id: TimerId) -> bool {
        let (slot_index, key, generation) = id.parts();
        let Some(slot) = self.slots.get_mut(slot_index) else {
            return false;
        };
        let Some(generations) = self.slot_generations.get_mut(slot_index) else {
            return false;
        };
        if key >= generations.len() || generations[key] != generation {
            return false;
        }
        if slot.contains(key) {
            slot.remove(key);
            self.pending_timers = self.pending_timers.saturating_sub(1);
            return true;
        }
        false
    }

    /// Expires all timers that are ready at the current time
    ///
    /// Expired timer wakers are moved to the provided ready vector
    pub fn expire(&mut self, now: Instant, ready: &mut Vec<Waker>) {
        debug_assert!(self.resolution_ms > 0, "Timer resolution must be non-zero");

        let target_tick = self.instant_to_tick(now);
        if target_tick < self.current_tick {
            return;
        }

        while self.current_tick <= target_tick {
            let slot_index = (self.current_tick % self.num_slots as u64) as usize;

            if let Some(slot) = self.slots.get_mut(slot_index) {
                slot.retain(|_, entry| {
                    if entry.remaining_rounds == 0 {
                        self.pending_timers = self.pending_timers.saturating_sub(1);
                        ready.push(std::mem::replace(&mut entry.waker, Waker::noop().clone()));
                        false
                    } else {
                        entry.remaining_rounds -= 1;
                        true
                    }
                });
            }

            self.current_tick = self.current_tick.saturating_add(1);
        }
    }

    /// Convert Instant to tick number (floor division) using nanosecond precision.
    /// Used in expire() to determine which ticks have passed.
    fn instant_to_tick(&self, at: Instant) -> u64 {
        let elapsed_ns = at.saturating_duration_since(self.start_time).as_nanos();
        let resolution_ns = (self.resolution_ms as u128) * 1_000_000;
        (elapsed_ns / resolution_ns) as u64
    }

    /// Convert Instant to tick number (ceiling division) using nanosecond precision.
    /// Used in schedule() to ensure the deadline tick strictly covers the target instant.
    /// Using `as_nanos()` instead of `as_millis()` prevents precision loss that would
    /// cause the timer to fire up to 1ms before the actual deadline.
    fn instant_to_tick_ceil(&self, at: Instant) -> u64 {
        let elapsed_ns = at.saturating_duration_since(self.start_time).as_nanos();
        let resolution_ns = (self.resolution_ms as u128) * 1_000_000;
        elapsed_ns.div_ceil(resolution_ns) as u64
    }

    /// Returns the number of pending timers across all slots
    pub fn pending_count(&self) -> usize {
        self.pending_timers
    }

    /// Returns whether any timers are pending.
    pub fn has_pending(&self) -> bool {
        self.pending_timers != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Waker;
    use std::time::Duration;

    fn create_test_waker() -> Waker {
        Waker::noop().clone()
    }

    #[test]
    fn test_timer_wheel_creation() {
        let wheel = TimerWheel::new(64, 10);
        assert_eq!(wheel.num_slots, 64);
        assert_eq!(wheel.resolution_ms, 10);
        assert_eq!(wheel.current_tick, 0);
        assert_eq!(wheel.pending_count(), 0);
    }

    #[test]
    fn test_timer_scheduling() {
        let mut wheel = TimerWheel::new(64, 1);
        let waker = create_test_waker();
        let now = Instant::now();
        let future_time = now + Duration::from_millis(50);

        let timer_id = wheel.schedule(future_time, &waker);
        assert_eq!(wheel.pending_count(), 1);

        // Test that we can schedule multiple timers
        let waker2 = create_test_waker();
        let timer_id2 = wheel.schedule(future_time + Duration::from_millis(10), &waker2);
        assert_eq!(wheel.pending_count(), 2);

        // Timer IDs should be different
        assert_ne!(timer_id, timer_id2);
    }

    #[test]
    fn test_timer_cancellation() {
        let mut wheel = TimerWheel::new(64, 1);
        let waker = create_test_waker();
        let future_time = Instant::now() + Duration::from_millis(50);

        let timer_id = wheel.schedule(future_time, &waker);
        assert_eq!(wheel.pending_count(), 1);

        // Cancel the timer
        assert!(wheel.cancel(timer_id));
        assert_eq!(wheel.pending_count(), 0);

        // Cancelling again should return false
        assert!(!wheel.cancel(timer_id));
    }

    #[test]
    fn test_timer_expiration() {
        let mut wheel = TimerWheel::new(64, 1);
        let waker = create_test_waker();
        let now = Instant::now();

        // Schedule a timer that should expire immediately
        wheel.schedule(now, &waker);
        assert_eq!(wheel.pending_count(), 1);

        let mut ready = Vec::new();
        wheel.expire(now + Duration::from_millis(5), &mut ready);

        assert_eq!(ready.len(), 1);
        assert_eq!(wheel.pending_count(), 0);
    }

    #[test]
    fn test_timer_wheel_advance() {
        let mut wheel = TimerWheel::new(10, 1);
        let now = wheel.start_time;

        // Schedule timers at different times
        let waker1 = create_test_waker();
        let waker2 = create_test_waker();
        let waker3 = create_test_waker();

        wheel.schedule(now + Duration::from_millis(1), &waker1);
        wheel.schedule(now + Duration::from_millis(2), &waker2);
        wheel.schedule(now + Duration::from_millis(5), &waker3);

        assert_eq!(wheel.pending_count(), 3);

        // Expire timers up to 3ms
        let mut ready = Vec::new();
        wheel.expire(now + Duration::from_millis(3), &mut ready);

        // Should have expired first 2 timers
        assert_eq!(ready.len(), 2);
        assert_eq!(wheel.pending_count(), 1);

        // Expire the remaining timer
        ready.clear();
        wheel.expire(now + Duration::from_millis(6), &mut ready);
        assert_eq!(ready.len(), 1);
        assert_eq!(wheel.pending_count(), 0);
    }

    #[test]
    fn test_timer_id_uniqueness() {
        let id1 = TimerId::new();
        let id2 = TimerId::new();
        let id3 = TimerId::new();

        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_timer_wheel_wrapping() {
        let mut wheel = TimerWheel::new(4, 1); // Small wheel for easy testing
        let now = wheel.start_time;

        // Schedule timers that will wrap around the wheel
        // Use times starting from 1ms to ensure they're in different slots from current
        let wakers: Vec<_> = (0..8).map(|_| create_test_waker()).collect();

        for (i, waker) in wakers.into_iter().enumerate() {
            wheel.schedule(now + Duration::from_millis((i + 1) as u64), &waker);
        }

        assert_eq!(wheel.pending_count(), 8);

        // Expire all timers by advancing significantly
        let mut ready = Vec::new();
        wheel.expire(now + Duration::from_millis(20), &mut ready);

        assert_eq!(ready.len(), 8);
        assert_eq!(wheel.pending_count(), 0);
    }

    #[test]
    fn test_timer_long_duration() {
        let mut wheel = TimerWheel::new(8, 1);
        let now = wheel.start_time;
        let waker = create_test_waker();

        // Schedule timer far in the future (multiple full rotations)
        wheel.schedule(now + Duration::from_millis(100), &waker);

        let mut ready = Vec::new();
        wheel.expire(now + Duration::from_millis(99), &mut ready);
        assert!(ready.is_empty());

        wheel.expire(now + Duration::from_millis(100), &mut ready);
        assert_eq!(ready.len(), 1);
        assert_eq!(wheel.pending_count(), 0);
    }
}
