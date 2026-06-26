//! A future that resolves when an I/O operation completes.
//!
//! Also checks the multi-shot accept result queue for the token,
//! enabling zero-syscall multi-shot accept handling.

use crate::cpu::{current_task_id, io_state};
use crate::io::{CompletionKind, IoError, IoToken};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A future that waits for an I/O operation, identified by an `IoToken`, to complete.
#[derive(Debug)]
pub struct IoFuture {
    token: IoToken,
}

impl IoFuture {
    /// Creates a new `IoFuture` for a given `IoToken`.
    #[inline(always)]
    pub fn new(token: IoToken) -> Self {
        Self { token }
    }
}

impl Future for IoFuture {
    type Output = Result<CompletionKind, IoError>;

    #[inline(always)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let state = io_state();

        // Check if the completion is already available in the standard map.
        // SAFETY: Single-threaded access.
        if let Some(result) = unsafe { state.take_completed(self.token) } {
            return Poll::Ready(result);
        }

        // Check the multi-shot accept result queue (fast None for non-accept ops).
        if let Some((_token, _op, result)) = state.io_backend.pop_accept_result(self.token) {
            return Poll::Ready(result);
        }

        // Store the waker so the runtime can wake us when completion arrives.
        // SAFETY: Single-threaded access.
        unsafe { state.store_waker(self.token, cx.waker().clone()) };

        // Register the current task_id for inline completion processing.
        if let Some(task_id) = current_task_id() {
            unsafe { state.store_pending_task(self.token, task_id) };
        }

        Poll::Pending
    }
}

impl Drop for IoFuture {
    fn drop(&mut self) {
        if let Some(state) = crate::cpu::try_io_state() {
            // SAFETY: Single-threaded access.
            unsafe {
                state.take_waker(self.token);
                state.take_completed(self.token);
                state.take_pending_task(self.token);
            }
        }
    }
}
