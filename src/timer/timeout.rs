use super::SleepFuture;
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

#[pin_project]
pub struct Timeout<F: Future> {
    #[pin]
    future: F,
    #[pin]
    timeout: SleepFuture,
}

impl<F: Future> Timeout<F> {
    pub fn new(future: F, duration: Duration) -> Self {
        let timeout = SleepFuture::new(duration);
        Timeout { future, timeout }
    }
}

impl<F: Future> Future for Timeout<F> {
    type Output = Result<F::Output, TimeoutError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        if let Poll::Ready(val) = this.future.poll(cx) {
            Poll::Ready(Ok(val))
        } else if let Poll::Ready(()) = this.timeout.poll(cx) {
            Poll::Ready(Err(TimeoutError))
        } else {
            Poll::Pending
        }
    }
}

#[derive(Debug)]
pub struct TimeoutError;
