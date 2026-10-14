//! A future that resolves when an I/O operation completes.
//!
//! Also checks the multi-shot accept result queue for the token,
//! enabling zero-syscall multi-shot accept handling.

use crate::cpu::{try_with_io_state, with_io_state};
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
    immediate: Option<Result<CompletionKind, IoError>>,
}

pub enum IoFutureInput {
    Token(IoToken),
    Error(IoError),
}

impl From<IoToken> for IoFutureInput {
    fn from(token: IoToken) -> Self {
        Self::Token(token)
    }
}

impl From<Result<IoToken, IoError>> for IoFutureInput {
    fn from(result: Result<IoToken, IoError>) -> Self {
        match result {
            Ok(token) => Self::Token(token),
            Err(error) => Self::Error(error),
        }
    }
}

impl IoFuture {
    /// Creates a new `IoFuture` for a given `IoToken`.
    #[inline(always)]
    pub fn new<T: Into<IoFutureInput>>(input: T) -> Self {
        match input.into() {
            IoFutureInput::Token(token) => Self {
                token,
                is_accept: false,
                completed: false,
                task_id: crate::cpu::current_task_id(),
                immediate: None,
            },
            IoFutureInput::Error(error) => Self::error(error),
        }
    }

    /// Creates an `IoFuture` for a multishot accept operation.
    #[inline(always)]
    pub fn accept<T: Into<IoFutureInput>>(input: T) -> Self {
        match input.into() {
            IoFutureInput::Token(token) => Self {
                token,
                is_accept: true,
                completed: false,
                task_id: crate::cpu::current_task_id(),
                immediate: None,
            },
            IoFutureInput::Error(error) => Self::error(error),
        }
    }

    /// Creates a future that is already failed before an operation was
    /// admitted to the backend. No token or slot is allocated in this path.
    #[inline(always)]
    pub fn error(error: IoError) -> Self {
        Self {
            token: IoToken::from_id(0),
            is_accept: false,
            completed: true,
            task_id: None,
            immediate: Some(Err(error)),
        }
    }
}

impl Future for IoFuture {
    type Output = Result<CompletionKind, IoError>;

    #[inline(always)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(result) = this.immediate.take() {
            return Poll::Ready(result);
        }
        with_io_state(|state| {
            // Owner-thread CQE delivery can hand the result directly to this
            // future, avoiding a completed-slot write followed by a second
            // token lookup. Remote/legacy paths continue below.
            // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
            if let Some(result) = unsafe { state.take_owner_completion(this.token) } {
                this.completed = true;
                return Poll::Ready(result);
            }
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
        })
    }
}

impl Drop for IoFuture {
    fn drop(&mut self) {
        if self.completed {
            return;
        }

        try_with_io_state(|state| {
            // SAFETY: Single-threaded access.
            unsafe { state.cancel_and_discard(self.token) }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::IoFuture;
    use crate::cpu::{test_io_state, RuntimeContextGuard};
    use crate::io::{CompletionKind, IoError, IoProvider, IoToken, Op};
    use crate::timer::TimerWheel;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    struct TestProvider;

    impl IoProvider for TestProvider {
        type Completion = (IoToken, Result<CompletionKind, IoError>);

        fn submit(&self, _op: Op) -> Result<IoToken, IoError> {
            Ok(IoToken::from_id(7))
        }

        fn drain_completions(&self) -> Poll<Vec<Self::Completion>> {
            Poll::Pending
        }
    }

    #[test]
    fn rejected_submit_future_returns_error_without_runtime_or_slot() {
        let mut future = IoFuture::error(IoError::Other("rejected".into()));
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(matches!(
            Pin::new(&mut future).poll(&mut cx),
            Poll::Ready(Err(IoError::Other(message))) if message == "rejected"
        ));
    }

    #[test]
    fn completion_is_delivered_once_through_the_real_future() {
        let state = test_io_state(TestProvider);
        let mut timers = TimerWheel::default();
        // SAFETY: the test keeps the state and timer on this owner thread
        // until the guard is dropped.
        let guard = unsafe { RuntimeContextGuard::enter(&state, &mut timers, Some(0)) };
        let token = state.io_backend.submit(Op::Nop).unwrap();
        let mut future = Box::pin(IoFuture::new(token));
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));
        // SAFETY: the test owns the shard-local state and token.
        unsafe {
            if let Some(waker) =
                state.insert_completed_and_take_waker(token, Ok(CompletionKind::Nop))
            {
                drop(waker);
            }
        }
        assert!(matches!(
            future.as_mut().poll(&mut cx),
            Poll::Ready(Ok(CompletionKind::Nop))
        ));
        // SAFETY: the test still owns the shard-local state.
        assert!(!unsafe { state.has_completed(token) });
        drop(guard);
    }

    #[test]
    fn completion_before_poll_is_delivered_without_registering_a_waker() {
        let state = test_io_state(TestProvider);
        let mut timers = TimerWheel::default();
        // SAFETY: the test owns the shard-local state and token.
        let guard = unsafe { RuntimeContextGuard::enter(&state, &mut timers, Some(0)) };
        let token = state.io_backend.submit(Op::Nop).unwrap();
        // SAFETY: the completion belongs to the owner-local token.
        unsafe {
            state.insert_completed_and_take_waker(token, Ok(CompletionKind::Nop));
        }
        let mut future = Box::pin(IoFuture::new(token));
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        assert!(matches!(
            future.as_mut().poll(&mut cx),
            Poll::Ready(Ok(CompletionKind::Nop))
        ));
        drop(guard);
    }

    #[test]
    fn dropping_pending_future_marks_completion_for_discard() {
        let state = test_io_state(TestProvider);
        let mut timers = TimerWheel::default();
        // SAFETY: the test keeps the state and timer on this owner thread
        // until the guard is dropped.
        let guard = unsafe { RuntimeContextGuard::enter(&state, &mut timers, Some(0)) };
        let token = state.io_backend.submit(Op::Nop).unwrap();
        let mut future = Box::pin(IoFuture::new(token));
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));
        drop(future);
        // SAFETY: the test owns the shard-local state and observes the owner
        // discard protocol before the guard is dropped.
        assert!(unsafe { state.take_discarded(token) });
        drop(guard);
    }
}
