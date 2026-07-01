use super::TimerId;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

/// A future that completes once the specified duration has elapsed.
#[derive(Debug)]
pub struct SleepFuture {
    deadline: Instant,
    timer_id: Option<TimerId>,
    registered_waker: Option<Waker>,
    fallback_triggered: bool,
}

impl SleepFuture {
    pub fn new(duration: Duration) -> Self {
        Self {
            deadline: Instant::now() + duration,
            timer_id: None,
            registered_waker: None,
            fallback_triggered: false,
        }
    }

    fn poll_inner(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.deadline {
            if let Some(id) = self.timer_id.take() {
                crate::cpu::cancel_timer_waker(id);
            }
            return Poll::Ready(());
        }

        let waker = cx.waker();
        let needs_reschedule = match &self.registered_waker {
            Some(existing) => !existing.will_wake(waker),
            None => true,
        };

        if needs_reschedule {
            if let Some(id) = self.timer_id.take() {
                crate::cpu::cancel_timer_waker(id);
            }

            let cached_waker = waker.clone();
            if let Some(id) = crate::cpu::register_timer_waker(self.deadline, &cached_waker) {
                self.timer_id = Some(id);
                self.registered_waker = Some(cached_waker);
            } else {
                self.registered_waker = Some(cached_waker.clone());
                if !self.fallback_triggered {
                    self.fallback_triggered = true;
                    let deadline = self.deadline;
                    let waker_clone = cached_waker;
                    std::thread::spawn(move || {
                        let now = Instant::now();
                        if deadline > now {
                            std::thread::sleep(deadline - now);
                        }
                        waker_clone.wake();
                    });
                }
            }
        }

        Poll::Pending
    }
}

impl Drop for SleepFuture {
    fn drop(&mut self) {
        if let Some(id) = self.timer_id.take() {
            crate::cpu::cancel_timer_waker(id);
        }
    }
}

impl Future for SleepFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: SleepFuture never moves its internal fields after pinning.
        let this = unsafe { self.get_unchecked_mut() };
        this.poll_inner(cx)
    }
}
