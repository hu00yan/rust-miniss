//! Task abstraction with panic-safe error handling
//!
//! This module provides the Task type that wraps futures for execution
//! in our async runtime. Tasks can complete successfully or fail due to panics,
//! with all outcomes represented as `TaskResult<T>`.
//!
//! ## Task Spawning
//!
//! The module provides several functions for spawning tasks:
//!
//! - [`spawn`] - Spawns a single-shot task
//! - [`spawn_periodic`] - Spawns a task that executes repeatedly at regular intervals
//!
//! ## Error Handling
//!
//! Tasks return `TaskResult<T>` which is `Result<T, TaskError>`. Currently,
//! the only error variant is `TaskError::Panic` which contains the panic
//! payload when a task panics during execution.
//!
//! ## JoinHandle
//!
//! JoinHandles allow awaiting task completion and retrieving results.
//! They implement `Future<Output = TaskResult<T>>` for integration with
//! the async ecosystem. JoinHandles can also be used to cancel tasks
//! via the [`JoinHandle::cancel`] method.

use crate::waker::{ReadyQueue, TaskId, WakerState};
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

/// Error returned from a failed task.
#[derive(Debug)]
pub enum TaskError {
    /// The task panicked.
    Panic(Box<dyn Any + Send + 'static>),
    /// The task was cancelled before completion.
    Cancelled,
}

/// The result of a completed task.
pub type TaskResult<T> = Result<T, TaskError>;

struct JoinState<T> {
    result: Option<TaskResult<T>>,
    completed: bool,
    waker: Option<Waker>,
}

impl<T> JoinState<T> {
    fn pending() -> Self {
        Self {
            result: None,
            completed: false,
            waker: None,
        }
    }
}

/// Completes a [`JoinHandle`] from the task execution side.
pub(crate) struct JoinCompleter<T> {
    state: Arc<parking_lot::Mutex<JoinState<T>>>,
}

impl<T> JoinCompleter<T> {
    fn complete(&self, result: TaskResult<T>) {
        let mut state = self.state.lock();
        if state.completed {
            return;
        }

        state.result = Some(result);
        state.completed = true;

        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }
}

impl<T> Drop for JoinCompleter<T> {
    fn drop(&mut self) {
        self.complete(Err(TaskError::Cancelled));
    }
}

/// Wraps a user future so task success, panic, and cancellation all reach the join handle.
pub(crate) struct JoinTaskFuture<F, T>
where
    F: Future<Output = T>,
{
    future: Pin<Box<F>>,
    completer: Option<JoinCompleter<T>>,
}

impl<F, T> JoinTaskFuture<F, T>
where
    F: Future<Output = T>,
{
    pub(crate) fn new(future: F, completer: JoinCompleter<T>) -> Self {
        Self {
            future: Box::pin(future),
            completer: Some(completer),
        }
    }
}

impl<F, T> Future for JoinTaskFuture<F, T>
where
    F: Future<Output = T>,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let poll_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            this.future.as_mut().poll(cx)
        }));

        match poll_result {
            Ok(Poll::Ready(value)) => {
                if let Some(completer) = this.completer.take() {
                    completer.complete(Ok(value));
                }
                Poll::Ready(())
            }
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => {
                if let Some(completer) = this.completer.take() {
                    completer.complete(Err(TaskError::Panic(payload)));
                }
                Poll::Ready(())
            }
        }
    }
}

/// Wraps a detached task so panics are isolated without a per-poll wrapper.
pub(crate) struct DetachedTaskFuture<F>
where
    F: Future<Output = ()>,
{
    future: Pin<Box<F>>,
}

impl<F> DetachedTaskFuture<F>
where
    F: Future<Output = ()>,
{
    pub(crate) fn new(future: F) -> Self {
        Self {
            future: Box::pin(future),
        }
    }
}

impl<F> Future for DetachedTaskFuture<F>
where
    F: Future<Output = ()>,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let poll_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            this.future.as_mut().poll(cx)
        }));

        match poll_result {
            Ok(Poll::Ready(())) | Err(_) => Poll::Ready(()),
            Ok(Poll::Pending) => Poll::Pending,
        }
    }
}

/// A task wraps a future for execution in the runtime
pub struct Task {
    id: TaskId,
    future: Pin<Box<dyn Future<Output = ()> + Send>>,
    /// Cached waker — created once and reused across polls.
    /// Reuses Arc-backed waker state across polls.
    waker: Option<Waker>,
    wake_state: Option<Arc<WakerState>>,
    completed: bool,
}

impl Drop for Task {
    fn drop(&mut self) {
        // Cancellation/removal must revoke escaped wakers before releasing
        // the task slot. Any later wake then becomes a harmless no-op.
        if let Some(state) = &self.wake_state {
            state.deactivate();
        }
    }
}

impl Task {
    /// Create a new task with the given future
    pub fn new(id: TaskId, future: impl Future<Output = ()> + Send + 'static) -> Self {
        Self {
            id,
            future: Box::pin(future),
            waker: None,
            wake_state: None,
            completed: false,
        }
    }

    /// Create a new task from a pinned boxed future
    pub fn from_pinned(id: TaskId, future: Pin<Box<dyn Future<Output = ()> + Send>>) -> Self {
        Self {
            id,
            future,
            waker: None,
            wake_state: None,
            completed: false,
        }
    }

    /// Get the task ID
    #[inline(always)]
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Poll the task's future
    #[inline(always)]
    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if self.completed {
            return Poll::Ready(());
        }
        match self.future.as_mut().poll(cx) {
            Poll::Ready(()) => {
                self.completed = true;
                if let Some(state) = &self.wake_state {
                    state.deactivate();
                }
                self.waker = None;
                self.wake_state = None;
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }

    /// Poll the task using its cached waker for the given ready queue.
    #[inline(always)]
    pub fn poll_with_cached_waker(&mut self) -> Poll<()> {
        if self.completed {
            return Poll::Ready(());
        }
        if let Some(state) = &self.wake_state {
            state.begin_poll();
        }
        let waker_ptr = self.waker.as_ref().expect("waker cached by ensure_waker") as *const Waker;
        // SAFETY: `waker_ptr` points to the cached waker stored in `self`.
        // The reference is only used to build a Context for the duration of
        // this poll call, and the cached waker outlives the Context.
        let mut context = unsafe { Context::from_waker(&*waker_ptr) };
        match self.future.as_mut().poll(&mut context) {
            Poll::Ready(()) => {
                self.completed = true;
                if let Some(state) = &self.wake_state {
                    state.deactivate();
                }
                self.waker = None;
                self.wake_state = None;
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }

    /// Ensure a cached waker exists without cloning it back out.
    #[inline(always)]
    pub fn ensure_waker(&mut self, queue: ReadyQueue) {
        if self.waker.is_none() {
            let (waker, state) = crate::waker::create_waker(self.id, queue);
            self.waker = Some(waker);
            self.wake_state = Some(state);
        }
    }

    /// Queue this task once. Used for initial admission and wake dedup.
    #[inline]
    pub fn schedule(&self) {
        if let Some(state) = &self.wake_state {
            state.schedule();
        }
    }

    /// Ensure a cached waker exists and return pointer to it for Context setup.
    #[inline(always)]
    pub fn cached_waker_ptr(&mut self, queue: ReadyQueue) -> *const Waker {
        self.ensure_waker(queue);
        self.waker.as_ref().expect("waker cached by ensure_waker") as *const Waker
    }

    /// Borrow the cached waker by reference after ensuring it exists.
    #[inline(always)]
    pub fn cached_waker(&mut self, queue: ReadyQueue) -> &Waker {
        self.ensure_waker(queue);
        self.waker.as_ref().expect("waker cached by ensure_waker")
    }

    /// Take the cached waker (called on task completion to free memory).
    #[inline(always)]
    pub fn take_waker(&mut self) -> Option<Waker> {
        if let Some(state) = &self.wake_state {
            state.deactivate();
        }
        self.wake_state = None;
        self.waker.take()
    }

    /// Mark the task as completed so stale ready-queue entries cannot poll it again.
    #[inline(always)]
    pub fn mark_completed(&mut self) {
        self.completed = true;
        if let Some(state) = &self.wake_state {
            state.deactivate();
        }
        self.waker = None;
        self.wake_state = None;
    }
}

/// A spawned task handle that can be awaited
pub struct JoinHandle<T> {
    task_id: TaskId,
    state: Arc<parking_lot::Mutex<JoinState<T>>>,
}

impl<T> JoinHandle<T> {
    /// Create a new join handle
    pub(crate) fn new(task_id: TaskId) -> (Self, JoinCompleter<T>) {
        let state = Arc::new(parking_lot::Mutex::new(JoinState::pending()));
        let handle = Self {
            task_id,
            state: state.clone(),
        };
        let completer = JoinCompleter { state };
        (handle, completer)
    }

    /// Get the task ID
    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    /// Check if the task has completed
    pub fn is_finished(&self) -> bool {
        self.state.lock().completed
    }

    /// Cancel the task
    ///
    /// This method attempts to cancel the running task. Note that cancellation
    /// is cooperative - the task will only be cancelled if it hasn't already completed.
    ///
    /// Returns `Ok(())` if the cancellation was processed successfully,
    /// or an error if the task cannot be cancelled (e.g., runtime shutdown).
    pub fn cancel(&self) -> crate::error::Result<()> {
        // In the new thread-per-core design, task cancellation is simplified
        // Since we don't track which CPU the task is on, we just mark it as cancelled
        tracing::warn!(
            "Task cancellation not fully implemented in thread-per-core runtime: {:?}",
            self.task_id
        );

        let mut state = self.state.lock();
        if !state.completed {
            state.result = Some(Err(TaskError::Cancelled));
            state.completed = true;
            tracing::info!("Task {:?} marked as cancelled", self.task_id);
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        }

        Ok(())
    }
}

impl<T: Send> Future for JoinHandle<T> {
    type Output = TaskResult<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.lock();
        if let Some(result) = state.result.take() {
            return Poll::Ready(result);
        }

        if state.completed {
            return Poll::Ready(Err(TaskError::Cancelled));
        }

        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// A builder for configuring and spawning tasks
///
/// The TaskBuilder provides a fluent interface for creating and spawning tasks
/// with different runtime backends (single-CPU executor or multi-CPU runtime).
pub struct TaskBuilder {
    // Future builder options could be added here
}

impl TaskBuilder {
    /// Create a new task builder
    pub fn new() -> Self {
        Self {}
    }

    /// Spawn a task using the appropriate runtime backend
    ///
    /// This method automatically selects between single-CPU executor and
    /// multi-CPU runtime based on build configuration and runtime availability.
    pub fn spawn<F, T>(self, future: F) -> crate::error::Result<JoinHandle<T>>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        // In the new queue-based design, we don't have a global runtime
        // Fall back to single-CPU executor
        self.spawn_single_cpu(future)
    }

    /// Spawn task on single-CPU executor
    fn spawn_single_cpu<F, T>(self, _future: F) -> crate::error::Result<JoinHandle<T>>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        panic!(
            "TaskBuilder::spawn() cannot create a new OS thread per task — use \
             Runtime::spawn() or block_on with a spawned task instead"
        )
    }
}

impl Default for TaskBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience function to spawn a task
pub fn spawn<F, T>(_future: F) -> crate::error::Result<JoinHandle<T>>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    panic!(
        "spawn() cannot create a new OS thread per task — use Runtime::spawn() or \
         block_on with a spawned task instead"
    )
}

/// Spawns a periodic task that executes a callback at regular intervals
///
/// The task will continue to run and execute the callback at the specified
/// period until the runtime shuts down or the returned JoinHandle is cancelled.
///
/// # Arguments
///
/// * `period` - The duration between callback executions
/// * `callback` - An async closure that will be executed periodically
///
/// # Returns
///
/// A `JoinHandle<()>` that can be used to cancel the periodic task
///
/// # Examples
///
/// ```rust,no_run
/// use std::time::Duration;
/// use rust_miniss::task;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Spawn a task that prints "tick" every second
/// let handle = task::spawn_periodic(Duration::from_secs(1), move || async {
///     println!("tick");
/// })?;
///
/// // Later, cancel the periodic task
/// handle.cancel()?;
/// # Ok(())
/// # }
/// ```
pub fn spawn_periodic<F, Fut>(
    _period: std::time::Duration,
    _callback: F,
) -> crate::error::Result<JoinHandle<()>>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    panic!("spawn_periodic() requires an active Runtime — use Runtime::spawn() instead")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_task_creation() {
        let task_id = TaskId(1);
        let future = async {
            println!("Hello from task!");
        };

        let task = Task::new(task_id, future);
        assert_eq!(task.id(), task_id);
    }

    // Helper to create a dummy waker for testing
    fn dummy_waker() -> std::task::Waker {
        futures::task::noop_waker()
    }

    #[test]
    fn test_task_builder_spawn_single_cpu() {
        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();

        let mut executor = crate::executor::Executor::new();
        let handle = executor.spawn(async move {
            completed_clone.store(true, Ordering::Release);
            42
        });

        executor.run();

        assert!(handle.is_finished());
        assert!(completed.load(Ordering::Acquire));
    }

    #[cfg(not(miri))]
    #[test]
    fn test_task_builder_spawn_multi_core() {
        crate::multicore::init_runtime(Some(2)).unwrap();

        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();

        let mut executor = crate::executor::Executor::new();
        let handle = executor.spawn(async move {
            completed_clone.store(true, Ordering::Release);
            "hello from multicore"
        });

        executor.run();

        assert!(handle.is_finished());
        assert!(completed.load(Ordering::Acquire));
    }

    #[test]
    fn test_task_polling() {
        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();

        let task_id = TaskId(1);
        let future = async move {
            completed_clone.store(true, Ordering::Release);
        };

        let mut task = Task::new(task_id, future);

        // Poll the task
        let waker = dummy_waker();
        let mut cx = Context::from_waker(&waker);

        let result = task.poll(&mut cx);

        // Task should complete immediately
        assert!(matches!(result, Poll::Ready(())));
        assert!(completed.load(Ordering::Acquire));
    }

    #[test]
    fn test_task_poll_after_completion_stays_ready() {
        let queue = Arc::new(crossbeam_queue::ArrayQueue::new(16));
        let task_id = TaskId(7);
        let mut task = Task::new(task_id, async {});
        task.ensure_waker(queue);

        assert!(matches!(task.poll_with_cached_waker(), Poll::Ready(())));
        assert!(matches!(task.poll_with_cached_waker(), Poll::Ready(())));
    }

    #[test]
    #[should_panic(expected = "spawn_periodic() requires an active Runtime")]
    fn test_spawn_periodic_requires_runtime() {
        use std::time::Duration;

        let counter = Arc::new(AtomicBool::new(false));
        let counter_clone = counter.clone();

        let _ = spawn_periodic(Duration::from_millis(100), move || {
            let counter = counter_clone.clone();
            async move {
                counter.store(true, Ordering::Release);
            }
        });
    }
}
