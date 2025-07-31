use std::time::{Duration, Instant};

pub struct Interval {
    period: Duration,
    next_tick: Instant,
}

impl Interval {
    pub fn new(duration: Duration) -> Self {
        Interval {
            period: duration,
            next_tick: Instant::now() + duration,
        }
    }

    pub async fn tick(&mut self) {
        loop {
            let now = Instant::now();
            if now >= self.next_tick {
                self.next_tick += self.period;
                return;
            } else {
                super::sleep::SleepFuture::new(self.next_tick - now).await;
            }
        }
    }
}
