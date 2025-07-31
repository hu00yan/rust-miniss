use super::TimerWheel;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use std::marker::PhantomPinned;

pub struct SleepFuture {
    wheel: Option<TimerWheel>,
    end_time: Instant,
    _pin: PhantomPinned,
}

impl SleepFuture {
    pub fn new(duration: Duration) -> Self {
        let wheel = TimerWheel::default();
        let end_time = Instant::now() + duration;
        Self {
            wheel: Some(wheel),
            end_time,
            _pin: PhantomPinned,
        }
    }

    fn poll_inner(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.end_time {
            Poll::Ready(())
        } else {
            if let Some(ref mut wheel) = self.wheel {
                wheel.schedule(self.end_time, cx.waker().clone());
            }
            Poll::Pending
        }
    }
}

impl Future for SleepFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { Pin::get_unchecked_mut(self) };
        this.poll_inner(cx)
    }
}
