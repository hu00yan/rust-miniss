//! A future that resolves when an I/O operation completes.

use crate::cpu::io_state;
use crate::io::{CompletionKind, IoError, IoToken};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A future that waits for an I/O operation, identified by an `IoToken`, to complete.
#[derive(Debug)]
pub struct IoFuture {
    token: IoToken,
    // We don't need a reference to the backend, just the token.
    // The result will be delivered to the CpuIoState by the runtime.
}

impl IoFuture {
    /// Creates a new `IoFuture` for a given `IoToken`.
    pub fn new(token: IoToken) -> Self {
        Self { token }
    }
}

impl Future for IoFuture {
    type Output = Result<CompletionKind, IoError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Get the I/O state for the current CPU thread.
        let state = io_state();

        // Check if the completion for our token is already available.
        if let Some(result) = state.completed_io.lock().unwrap().remove(&self.token) {
            // The operation is complete, return the result.
            return Poll::Ready(result);
        }

        // The operation is not yet complete. Register the waker so the runtime
        // can wake us up when the completion arrives.
        state
            .io_wakers
            .lock()
            .unwrap()
            .insert(self.token, cx.waker().clone());

        // Return Pending to indicate that we are still waiting.
        Poll::Pending
    }
}

impl Drop for IoFuture {
    fn drop(&mut self) {
        // When the future is dropped, we need to clean up any associated state.
        // The most important thing is to remove the waker, if it exists, to prevent
        // a memory leak. A more advanced implementation would also attempt to
        // cancel the underlying I/O operation.
        let state = io_state();
        state.io_wakers.lock().unwrap().remove(&self.token);
        // We also remove any potential completion that might have arrived just now.
        state.completed_io.lock().unwrap().remove(&self.token);
    }
}
