#![no_main]
use libfuzzer_sys::fuzz_target;
use rust_miniss::timer::{TimerWheel, TimerId};
use std::time::{Duration, Instant};
use std::sync::Arc;
use std::task::{Wake, Waker};

struct NoopWaker;
impl Wake for NoopWaker { fn wake(self: Arc<Self>) {} }
fn waker() -> Waker { Arc::new(NoopWaker).into() }

fuzz_target!(|input: (u8, u16, u16)| {
    let (slots, res_ms, ops) = input;
    let num_slots = 1 + (slots as usize % 128);
    let resolution_ms = 1 + (res_ms as u64 % 10);
    let mut wheel = TimerWheel::new(num_slots, resolution_ms);
    let now = Instant::now();

    // Perform a sequence of schedule/cancel/expire operations
    let mut ids: Vec<TimerId> = Vec::new();
    for i in 0..(ops as usize % 256) {
        let when = now + Duration::from_millis((i as u64 % 1000) + 1);
        let id = wheel.schedule(when, waker());
        ids.push(id);
        if i % 3 == 0 {
            let _ = wheel.cancel(id);
        }
    }

    let mut ready = Vec::new();
    // Advance time beyond all timers
    wheel.expire(now + Duration::from_secs(2), &mut ready);
});
