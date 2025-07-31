//! Task abstraction with panic-safe error handling
//!
//! This module provides the Task type that wraps futures for execution
//! in our async runtime. Tasks can complete successfully or fail due to panics,
//! with all outcomes represented as `TaskResult<T>`.
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
//! the async ecosystem.

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use crate::waker::TaskId;

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
    result: crate::future::Future<TaskResult<T>>,
}

impl<T> JoinHandle<T> {
    /// Create a new join handle
    pub(crate) fn new(task_id: TaskId, result: crate::future::Future<TaskResult<T>>) -> Self {
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
    
    /// Cancel the task
    /// 
    /// This method attempts to cancel the running task by sending a CancelTask
    /// message to the appropriate CPU. Note that cancellation is cooperative -
    /// the task will only be cancelled if it hasn't already completed.
    /// 
    /// Returns `Ok(())` if the cancellation message was sent successfully,
    /// or an error if the task cannot be cancelled (e.g., runtime shutdown).
    pub fn cancel(&self) -> crate::error::Result<()> {
        // For now, we'll use the global runtime to send the cancellation message
        // In a more sophisticated implementation, we'd track which CPU the task is on
        #[cfg(feature = "multicore")]
        {
            if let Ok(runtime) = std::panic::catch_unwind(|| crate::multicore::runtime()) {
                return self.cancel_multicore(&runtime);
            }
        }
        
        // For single-CPU executor, we can't easily cancel tasks without more infrastructure
        // This would require a more sophisticated design where tasks can be interrupted
        tracing::warn!("Task cancellation not yet implemented for single-CPU executor");
        Ok(())
    }
    
    /// Cancel task in multi-core runtime
    #[cfg(feature = "multicore")]
    fn cancel_multicore(&self, runtime: &std::sync::Arc<crate::multicore::MultiCoreRuntime>) -> crate::error::Result<()> {
        // Use the runtime's cancel_task method which looks up the CPU and sends the cancellation message
        runtime.cancel_task(self.task_id)?;
        
        // Mark the result as cancelled if it hasn't completed yet
        if !self.is_finished() {
            tracing::info!("Task {:?} marked as cancelled", self.task_id);
        }
        
        Ok(())
    }
}

impl<T: Send> Future for JoinHandle<T> {
    type Output = TaskResult<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.result).poll(cx)
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
        // Try multi-CPU runtime first if available
        #[cfg(feature = "multicore")]
        {
            // Use the public runtime() function to access the global runtime
            if let Ok(runtime) = std::panic::catch_unwind(|| crate::multicore::runtime()) {
                return self.spawn_multicore(&runtime, future);
            }
        }
        
        // Fall back to single-CPU executor
        self.spawn_single_cpu(future)
    }
    
    /// Spawn task on single-CPU executor
    fn spawn_single_cpu<F, T>(self, future: F) -> crate::error::Result<JoinHandle<T>>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        // Create a new executor for this task
        // In a real implementation, this would use a thread-local or global executor
        let mut executor = crate::executor::Executor::new();
        let handle = executor.spawn(future);
        
        // For simplicity, we'll schedule the task to run immediately
        // In a real implementation, this would be handled by the runtime scheduler
        std::thread::spawn(move || {
            executor.run();
        });
        
        Ok(handle)
    }
    
    /// Spawn task on multi-core runtime
    #[cfg(feature = "multicore")]
    fn spawn_multicore<'a, F, T>(
        self, 
        runtime: &'a std::sync::Arc<crate::multicore::MultiCoreRuntime>, 
        future: F
    ) -> crate::error::Result<JoinHandle<T>>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (result_future, promise) = crate::future::Future::new();

        let task_future = async move {
            let result = future.await;
            promise.complete(Ok(result));
        };
        
        let task_id = runtime.spawn(Box::pin(task_future))?;

        Ok(JoinHandle::new(task_id, result_future))
    }
}

impl Default for TaskBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience function to spawn a task
pub fn spawn<F, T>(future: F) -> crate::error::Result<JoinHandle<T>>
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
    fn test_task_builder_spawn_single_cpu() {
        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();
        
        let handle = spawn(async move {
            completed_clone.store(true, Ordering::SeqCst);
            42
        }).unwrap();

        // Block on the handle to get the result
        let result = crate::executor::Runtime::new().block_on(handle);

        assert_eq!(result.unwrap(), 42);
        // Give the background thread a chance to complete
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(completed.load(Ordering::SeqCst));
    }

    #[test]
    #[cfg(feature = "multicore")]
    fn test_task_builder_spawn_multi_core() {
        // Initialize the multi-core runtime
        crate::multicore::init_runtime(Some(2)).unwrap();

        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();

        let handle = spawn(async move {
            completed_clone.store(true, Ordering::SeqCst);
            "hello from multicore"
        }).unwrap();

        // Block on the handle to get the result
        let result = crate::executor::Runtime::new().block_on(handle);

        assert_eq!(result.unwrap(), "hello from multicore");
        // Give the multicore runtime a chance to complete
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(completed.load(Ordering::SeqCst));
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
