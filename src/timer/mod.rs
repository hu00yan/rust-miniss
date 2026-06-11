use std::collections::{HashMap, VecDeque};
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

/// A high-performance timer wheel implementation for scheduling timeouts
///
/// This implementation uses a circular buffer of timer slots with minimal
/// unsafe code for performance-critical slot indexing. Includes a HashMap
/// for O(1) timer cancellation.
pub struct TimerWheel {
    slots: Vec<VecDeque<Entry>>,
    resolution_ms: u64,
    num_slots: usize,
    current_tick: u64,
    start_time: Instant,
    /// Maps TimerId to slot index for cancellation
    timer_locations: HashMap<TimerId, usize>,
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
        for _ in 0..num_slots {
            slots.push(VecDeque::with_capacity(
                crate::config::EXPECTED_WAKEUP_COUNT,
            ));
        }

        Self {
            slots,
            resolution_ms,
            num_slots,
            current_tick: 0,
            start_time: Instant::now(),
            timer_locations: HashMap::new(),
        }
    }

    /// Schedules a timer to expire at the specified time
    ///
    /// Returns a TimerId that can be used to cancel the timer
    pub fn schedule(&mut self, at: Instant, waker: Waker) -> TimerId {
        let timer_id = TimerId::new();
        debug_assert!(self.resolution_ms > 0, "Timer resolution must be non-zero");

        let deadline_tick = self.instant_to_tick(at).max(self.current_tick);
        let distance = deadline_tick.saturating_sub(self.current_tick);
        let rounds = distance / self.num_slots as u64;
        let slot_index = ((self.current_tick + distance) % self.num_slots as u64) as usize;

        let entry = Entry {
            id: timer_id,
            waker,
            remaining_rounds: rounds,
        };

        self.slots
            .get_mut(slot_index)
            .expect("slot index must be valid")
            .push_back(entry);

        self.timer_locations.insert(timer_id, slot_index);
        timer_id
    }

    /// Attempts to cancel a timer with the given ID
    ///
    /// Returns true if the timer was found and cancelled, false otherwise
    pub fn cancel(&mut self, id: TimerId) -> bool {
        if let Some(slot_index) = self.timer_locations.remove(&id) {
            if let Some(slot) = self.slots.get_mut(slot_index) {
                for idx in 0..slot.len() {
                    if slot[idx].id == id {
                        slot.remove(idx);
                        return true;
                    }
                }
            }
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
                let mut remaining = slot.len();
                while remaining > 0 {
                    if let Some(mut entry) = slot.pop_front() {
                        if entry.remaining_rounds == 0 {
                            self.timer_locations.remove(&entry.id);
                            ready.push(entry.waker);
                        } else {
                            entry.remaining_rounds -= 1;
                            slot.push_back(entry);
                        }
                    }
                    remaining -= 1;
                }
            }

            self.current_tick = self.current_tick.saturating_add(1);
        }
    }

    fn instant_to_tick(&self, at: Instant) -> u64 {
        let elapsed_ms = at.saturating_duration_since(self.start_time).as_millis();
        let resolution = self.resolution_ms as u128;
        (elapsed_ms / resolution) as u64
    }

    /// Returns the number of pending timers across all slots
    pub fn pending_count(&self) -> usize {
        self.slots.iter().map(|slot| slot.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::task::Wake;
    use std::time::Duration;

    struct TestWaker;

    impl Wake for TestWaker {
        fn wake(self: Arc<Self>) {}
    }

    fn create_test_waker() -> Waker {
        Arc::new(TestWaker).into()
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

        let timer_id = wheel.schedule(future_time, waker);
        assert_eq!(wheel.pending_count(), 1);

        // Test that we can schedule multiple timers
        let waker2 = create_test_waker();
        let timer_id2 = wheel.schedule(future_time + Duration::from_millis(10), waker2);
        assert_eq!(wheel.pending_count(), 2);

        // Timer IDs should be different
        assert_ne!(timer_id, timer_id2);
    }

    #[test]
    fn test_timer_cancellation() {
        let mut wheel = TimerWheel::new(64, 1);
        let waker = create_test_waker();
        let future_time = Instant::now() + Duration::from_millis(50);

        let timer_id = wheel.schedule(future_time, waker);
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
        wheel.schedule(now, waker);
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

        wheel.schedule(now + Duration::from_millis(1), waker1);
        wheel.schedule(now + Duration::from_millis(2), waker2);
        wheel.schedule(now + Duration::from_millis(5), waker3);

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
            wheel.schedule(now + Duration::from_millis((i + 1) as u64), waker);
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
        wheel.schedule(now + Duration::from_millis(100), waker);

        let mut ready = Vec::new();
        wheel.expire(now + Duration::from_millis(99), &mut ready);
        assert!(ready.is_empty());

        wheel.expire(now + Duration::from_millis(100), &mut ready);
        assert_eq!(ready.len(), 1);
        assert_eq!(wheel.pending_count(), 0);
    }
}
