//! Task abstraction
//! 
//! This module provides the Task type that wraps futures for execution
//! in our async runtime.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use crate::waker::TaskId;

/// A task wraps a future for execution in the runtime
pub struct Task {
    id: TaskId,
    future: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl Task {
    /// Create a new task with the given future
    pub fn new(id: TaskId, future: impl Future<Output = ()> + Send + 'static) -> Self {
        Self {
            id,
            future: Box::pin(future),
        }
    }

    /// Create a new task from a pinned boxed future
    pub fn from_pinned(id: TaskId, future: Pin<Box<dyn Future<Output = ()> + Send>>) -> Self {
        Self {
            id,
            future,
        }
    }

    /// Get the task ID
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Poll the task's future
    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        self.future.as_mut().poll(cx)
    }
}

/// A spawned task handle that can be awaited
pub struct JoinHandle<T> {
    task_id: TaskId,
    result: crate::future::Future<T>,
}

impl<T> JoinHandle<T> {
    /// Create a new join handle
    pub(crate) fn new(task_id: TaskId, result: crate::future::Future<T>) -> Self {
        Self {
            task_id,
            result,
        }
    }

    /// Get the task ID
    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    /// Check if the task has completed
    pub fn is_finished(&self) -> bool {
        self.result.is_ready()
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.result).poll(cx)
    }
}

/// A task builder for customizing task properties
pub struct TaskBuilder {
    name: Option<String>,
}

impl TaskBuilder {
    /// Create a new task builder
    pub fn new() -> Self {
        Self { name: None }
    }

    /// Set the task name for debugging
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Spawn the task with the given future
    pub fn spawn<F, T>(self, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        // For now, we'll use a simple counter for task IDs
        // In a real implementation, this would be managed by the executor
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let task_id = TaskId(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst));

        let (result_future, promise) = crate::future::Future::new();

        // Wrap the user's future to complete our promise when done
        let _wrapped_future = async move {
            let result = future.await;
            promise.complete(result);
        };

        // TODO: In a complete implementation, we would register this task
        // with the executor here. For now, we'll return the handle.
        
        JoinHandle::new(task_id, result_future)
    }
}

impl Default for TaskBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn a new task
pub fn spawn<F, T>(future: F) -> JoinHandle<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    TaskBuilder::new().spawn(future)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn test_task_creation() {
        let task_id = TaskId(1);
        let future = async { println!("Hello from task!"); };
        
        let task = Task::new(task_id, future);
        assert_eq!(task.id(), task_id);
    }

    #[test]
    fn test_join_handle() {
        let handle = spawn(async { 42 });
        
        // Initially not finished
        assert!(!handle.is_finished());
        
        // Task ID should be assigned
        let task_id = handle.task_id();
        assert!(task_id.0 > 0);
    }

    #[test]
    fn test_task_builder() {
        let handle = TaskBuilder::new()
            .name("test-task")
            .spawn(async { "result" });
        
        assert!(!handle.is_finished());
    }

    // Helper to create a dummy waker for testing
    fn dummy_waker() -> std::task::Waker {
        use std::task::{RawWaker, RawWakerVTable};

        fn dummy_clone(_: *const ()) -> RawWaker {
            dummy_raw_waker()
        }
        fn dummy_wake(_: *const ()) {}
        fn dummy_wake_by_ref(_: *const ()) {}
        fn dummy_drop(_: *const ()) {}

        fn dummy_raw_waker() -> RawWaker {
            RawWaker::new(
                std::ptr::null(),
                &RawWakerVTable::new(dummy_clone, dummy_wake, dummy_wake_by_ref, dummy_drop),
            )
        }

        unsafe { std::task::Waker::from_raw(dummy_raw_waker()) }
    }

    #[test]
    fn test_task_polling() {
        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();
        
        let task_id = TaskId(1);
        let future = async move {
            completed_clone.store(true, Ordering::SeqCst);
        };
        
        let mut task = Task::new(task_id, future);
        
        // Poll the task
        let waker = dummy_waker();
        let mut cx = Context::from_waker(&waker);
        
        let result = task.poll(&mut cx);
        
        // Task should complete immediately
        assert!(matches!(result, Poll::Ready(())));
        assert!(completed.load(Ordering::SeqCst));
    }
}
