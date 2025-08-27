//! Cancellation utilities for cooperative task cancellation
//!
//! This module provides utilities for implementing cooperative task cancellation.
//! Tasks should periodically check for cancellation and exit early when requested.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A token that can be used to signal cancellation to a task
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Create a new cancellation token
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Cancel the token
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Check if cancellation has been requested
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
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
        CancellableFuture {
            inner: self,
            token,
        }
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
        let this = unsafe { self.get_unchecked_mut() };
        
        // Check for cancellation
        if this.token.is_cancelled() {
            return std::task::Poll::Ready(Err(crate::task::TaskError::Cancelled));
        }

        // Poll the inner future
        match unsafe { std::pin::Pin::new_unchecked(&mut this.inner) }.poll(cx) {
            std::task::Poll::Ready(value) => std::task::Poll::Ready(Ok(value)),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}