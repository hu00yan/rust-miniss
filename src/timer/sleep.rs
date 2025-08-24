use crate::runtime_context;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use std::marker::PhantomPinned;

pub struct SleepFuture {
    end_time: Instant,
    _pin: PhantomPinned,
}

impl SleepFuture {
    pub fn new(duration: Duration) -> Self {
        let end_time = Instant::now() + duration;
        Self {
            end_time,
            _pin: PhantomPinned,
        }
    }

    fn poll_inner(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.end_time {
            Poll::Ready(())
        } else {
            runtime_context::with_executor(|executor| {
                executor
                    .timer_wheel
                    .lock()
                    .unwrap()
                    .schedule(self.end_time, cx.waker().clone());
            });
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
