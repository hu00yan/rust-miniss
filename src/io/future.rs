//! A future that resolves when an I/O operation completes.
//!
//! Also checks the multi-shot accept result queue for the token,
//! enabling zero-syscall multi-shot accept handling.

use crate::cpu::io_state;
use crate::io::{CompletionKind, IoError, IoToken};
use crate::waker::TaskId;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A future that waits for an I/O operation, identified by an `IoToken`, to complete.
#[derive(Debug)]
pub struct IoFuture {
    token: IoToken,
    is_accept: bool,
    completed: bool,
    task_id: Option<TaskId>,
}

impl IoFuture {
    /// Creates a new `IoFuture` for a given `IoToken`.
    #[inline(always)]
    pub fn new(token: IoToken) -> Self {
        Self {
            token,
            is_accept: false,
            completed: false,
            task_id: crate::cpu::current_task_id(),
        }
    }

    /// Creates an `IoFuture` for a multishot accept operation.
    #[inline(always)]
    pub fn accept(token: IoToken) -> Self {
        Self {
            token,
            is_accept: true,
            completed: false,
            task_id: crate::cpu::current_task_id(),
        }
    }
}

impl Future for IoFuture {
    type Output = Result<CompletionKind, IoError>;

    #[inline(always)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let state = io_state();

        // Check if the completion is already available in the standard map.
        // SAFETY: Single-threaded access.
        if let Some(result) = unsafe { state.take_completed(this.token) } {
            this.completed = true;
            return Poll::Ready(result);
        }

        if this.is_accept {
            // Check the multi-shot accept result queue.
            if let Some(result) = state.io_backend.pop_accept_result(this.token) {
                this.completed = true;
                return Poll::Ready(result);
            }
        }

        // Store the waker and cached task id in one slot lookup.
        // SAFETY: Single-threaded access.
        unsafe { state.store_waker_and_pending_task(this.token, cx.waker(), this.task_id) };

        Poll::Pending
    }
}

impl Drop for IoFuture {
    fn drop(&mut self) {
        if self.completed {
            return;
        }

        if let Some(state) = crate::cpu::try_io_state() {
            // SAFETY: Single-threaded access.
            unsafe {
                state.clear_slot(self.token);
            }
        }
    }
}
