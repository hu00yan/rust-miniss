//! Cancellation utilities for cooperative task cancellation
//!
//! This module provides utilities for implementing cooperative task cancellation.
//! Tasks should periodically check for cancellation and exit early when requested.

use pin_project::pin_project;

#[cfg(loom)]
use loom::sync::atomic::{AtomicBool, Ordering};
#[cfg(loom)]
use loom::sync::Arc;
#[cfg(not(loom))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(loom))]
use std::sync::Arc;
use std::task::Waker;

#[cfg(loom)]
type WaiterMutex<T> = loom::sync::Mutex<T>;
#[cfg(not(loom))]
type WaiterMutex<T> = parking_lot::Mutex<T>;

#[derive(Debug)]
struct CancellationState {
    cancelled: AtomicBool,
    waiter: WaiterMutex<Option<Waker>>,
}

impl CancellationState {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            waiter: WaiterMutex::new(None),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake_waiter();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn register(&self, waker: &Waker) {
        let mut waiter = self.lock_waiter();
        if self.is_cancelled() {
            drop(waiter);
            waker.wake_by_ref();
        } else {
            *waiter = Some(waker.clone());
        }
    }

    fn wake_waiter(&self) {
        let waiter = self.lock_waiter().take();
        if let Some(waker) = waiter {
            waker.wake();
        }
    }

    #[cfg(loom)]
    fn lock_waiter(&self) -> loom::sync::MutexGuard<'_, Option<Waker>> {
        self.waiter.lock().expect("cancellation waiter mutex")
    }

    #[cfg(not(loom))]
    fn lock_waiter(&self) -> parking_lot::MutexGuard<'_, Option<Waker>> {
        self.waiter.lock()
    }
}

/// A token that can be used to signal cancellation to a task
#[derive(Clone, Debug)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

impl CancellationToken {
    /// Create a new cancellation token
    pub fn new() -> Self {
        Self {
            state: Arc::new(CancellationState::new()),
        }
    }

    /// Cancel the token
    pub fn cancel(&self) {
        self.state.cancel();
    }

    /// Check if cancellation has been requested
    pub fn is_cancelled(&self) -> bool {
        self.state.is_cancelled()
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
///
/// The only field that participates in pinning is `inner`: this type delegates
/// the projection to it and never re-borrows it independently. Declaring that
/// with `pin_project` is what lets `poll` reach the inner future without an
/// `unsafe` projection — the projection's soundness is structural instead of a
/// comment.
#[pin_project]
pub struct CancellableFuture<F> {
    #[pin]
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
        // Structural projection: `inner` stays pinned, `token` is reachable as
        // a plain reference. No hand-written unsafe projection is needed.
        let this = self.project();

        // Check for cancellation
        if this.token.is_cancelled() {
            return std::task::Poll::Ready(Err(crate::task::TaskError::Cancelled));
        }

        // Register before the second check: cancellation racing this poll
        // either sees the registered waker or is observed by this recheck.
        this.token.state.register(cx.waker());
        if this.token.is_cancelled() {
            return std::task::Poll::Ready(Err(crate::task::TaskError::Cancelled));
        }

        // Poll the inner future. `project()` keeps `inner` pinned structurally,
        // so this delegation is sound without a hand-written projection.
        match this.inner.poll(cx) {
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
    use std::sync::Arc as StdArc;
    use std::task::{Context, Poll};

    struct Never;
    impl Future for Never {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            Poll::Pending
        }
    }

    struct WakeCounter(StdArc<AtomicUsize>);
    impl futures::task::ArcWake for WakeCounter {
        fn wake_by_ref(this: &StdArc<Self>) {
            this.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[test]
    fn cancellation_wakes_permanently_pending_future() {
        let token = CancellationToken::new();
        let mut future = Box::pin(Never.cancellable(token.clone()));
        let wakes = StdArc::new(AtomicUsize::new(0));
        let waker = futures::task::waker(StdArc::new(WakeCounter(wakes.clone())));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));
        token.cancel();
        assert!(wakes.load(Ordering::Acquire) > 0);
        assert!(matches!(
            future.as_mut().poll(&mut cx),
            Poll::Ready(Err(crate::task::TaskError::Cancelled))
        ));
    }

    #[cfg(loom)]
    #[test]
    fn loom_cancel_races_with_waiter_registration() {
        loom::model(|| {
            let token = CancellationToken::new();
            let cancelling = token.clone();
            let wakes = StdArc::new(AtomicUsize::new(0));
            let waker = futures::task::waker(StdArc::new(WakeCounter(wakes.clone())));
            let thread = loom::thread::spawn(move || cancelling.cancel());

            token.state.register(&waker);
            thread.join().expect("cancellation thread");

            assert!(token.is_cancelled());
            assert!(wakes.load(Ordering::Acquire) > 0);
        });
    }
}
