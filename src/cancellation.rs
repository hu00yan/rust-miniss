//! Cancellation utilities for cooperative task cancellation
//!
//! This module provides utilities for implementing cooperative task cancellation.
//! Tasks should periodically check for cancellation and exit early when requested.

use futures::task::AtomicWaker;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A token that can be used to signal cancellation to a task
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    waker: Arc<AtomicWaker>,
}

impl CancellationToken {
    /// Create a new cancellation token
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            waker: Arc::new(AtomicWaker::new()),
        }
    }

    /// Cancel the token
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.waker.wake();
    }

    /// Check if cancellation has been requested
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Extension trait for futures that supports cancellation
pub trait CancellableFutureExt<T>: Sized {
    /// Wrap the future with a cancellation token
    fn cancellable(self, token: CancellationToken) -> CancellableFuture<Self> {
        CancellableFuture { inner: self, token }
    }
}

impl<F, T> CancellableFutureExt<T> for F where F: std::future::Future<Output = T> {}

/// A future that can be cancelled
pub struct CancellableFuture<F> {
    inner: F,
    token: CancellationToken,
}

impl<F, T> std::future::Future for CancellableFuture<F>
where
    F: std::future::Future<Output = T>,
{
    type Output = Result<T, crate::task::TaskError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // SAFETY: CancellableFuture does not move `inner` after being pinned;
        // the projection only exposes fields through this pinned poll call.
        let this = unsafe { self.get_unchecked_mut() };

        // Check for cancellation
        if this.token.is_cancelled() {
            return std::task::Poll::Ready(Err(crate::task::TaskError::Cancelled));
        }

        // Register before the second check: cancellation racing this poll
        // either sees the registered waker or is observed by this recheck.
        this.token.waker.register(cx.waker());
        if this.token.is_cancelled() {
            return std::task::Poll::Ready(Err(crate::task::TaskError::Cancelled));
        }

        // Poll the inner future
        // SAFETY: `inner` is pinned in place by the parent CancellableFuture
        // and is not moved before or after this delegated poll.
        match unsafe { std::pin::Pin::new_unchecked(&mut this.inner) }.poll(cx) {
            std::task::Poll::Ready(value) => std::task::Poll::Ready(Ok(value)),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    struct Never;
    impl Future for Never {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            Poll::Pending
        }
    }

    struct WakeCounter(Arc<AtomicUsize>);
    impl futures::task::ArcWake for WakeCounter {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[test]
    fn cancellation_wakes_permanently_pending_future() {
        let token = CancellationToken::new();
        let mut future = Box::pin(Never.cancellable(token.clone()));
        let wakes = Arc::new(AtomicUsize::new(0));
        let waker = futures::task::waker(Arc::new(WakeCounter(wakes.clone())));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));
        token.cancel();
        assert!(wakes.load(Ordering::Acquire) > 0);
        assert!(matches!(
            future.as_mut().poll(&mut cx),
            Poll::Ready(Err(crate::task::TaskError::Cancelled))
        ));
    }
}
