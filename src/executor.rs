//! Single-threaded executor
//! 
//! This module provides a basic single-threaded async executor that can
//! run futures to completion.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use crossbeam_queue::SegQueue;

use crate::task::{Task, JoinHandle};
use crate::waker::{MinissWaker, TaskId};

/// A single-threaded async runtime
pub struct Runtime {
    executor: Mutex<Executor>,
}

impl Runtime {
    /// Create a new runtime
    pub fn new() -> Self {
        Self {
            executor: Mutex::new(Executor::new()),
        }
    }

    /// Run a future to completion
    pub fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        self.executor.lock().unwrap().block_on(future)
    }

    /// Spawn a new task
    pub fn spawn<F, T>(&self, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.executor.lock().unwrap().spawn(future)
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

/// The core executor that manages task scheduling
pub struct Executor {
    tasks: HashMap<TaskId, Task>,
    ready_queue: Arc<SegQueue<TaskId>>,
    next_task_id: AtomicU64,
}

impl Executor {
    /// Create a new executor
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            ready_queue: Arc::new(SegQueue::new()),
            next_task_id: AtomicU64::new(1),
        }
    }

    /// Run a future to completion
    pub fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        // Pin the future to the stack
        let mut future = Box::pin(future);

        // Waker that unparks the current thread when the future makes progress.
        // This uses the `ArcWake` trait to create a waker that holds a reference
        // to the thread parker.
        struct Parker(std::thread::Thread);
        impl futures::task::ArcWake for Parker {
            fn wake_by_ref(arc_self: &Arc<Self>) {
                arc_self.0.unpark();
            }
        }
        let waker = futures::task::waker(Arc::new(Parker(std::thread::current())));
        let mut context = Context::from_waker(&waker);

        // Poll the future until it completes, parking the thread when pending
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => {
                    // Park the thread until it is woken
                    std::thread::park();
                }
            }
        }
    }

    /// Spawn a new task
    pub fn spawn<F, T>(&mut self, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let task_id = TaskId(self.next_task_id.fetch_add(1, Ordering::SeqCst));

        let (result_future, promise) = crate::future::Future::new();

        // Wrap the user's future to complete our promise when done
        let wrapped_future = async move {
            let result = future.await;
            promise.complete(result);
        };

        // Create the task
        let task = Task::new(task_id, wrapped_future);
        
        // Add to our task list and ready queue
        self.tasks.insert(task_id, task);
        self.ready_queue.push(task_id);

        JoinHandle::new(task_id, result_future)
    }

    /// Run all ready tasks once
    pub fn tick(&mut self) -> bool {
        let mut made_progress = false;

        while let Some(task_id) = self.ready_queue.pop() {
            if let Some(mut task) = self.tasks.remove(&task_id) {
                // Create a waker for this task
                let waker = MinissWaker::new(task_id, self.ready_queue.clone());
                let mut context = Context::from_waker(&waker);

                // Poll the task
                match task.poll(&mut context) {
                    Poll::Ready(()) => {
                        // Task completed, don't put it back
                        made_progress = true;
                    }
                    Poll::Pending => {
                        // Task is still pending, put it back
                        self.tasks.insert(task_id, task);
                        made_progress = true;
                    }
                }
            }
        }

        made_progress
    }

    /// Run the executor until all tasks complete
    pub fn run(&mut self) {
        use std::time::Duration;
        while !self.tasks.is_empty() {
            if !self.tick() {
                // No progress made; park the thread briefly to avoid busy loop
                std::thread::park_timeout(Duration::from_millis(1));
            }
        }
    }

    /// Get the number of active tasks
    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

/// Create a dummy waker for futures that don't need to be woken

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    #[test]
    fn test_runtime_creation() {
        let _runtime = Runtime::new();
        // Runtime should be created successfully
    }

    #[test]
    fn test_block_on_immediate() {
        let runtime = Runtime::new();
        let result = runtime.block_on(async { 42 });
        assert_eq!(result, 42);
    }

    #[test]
    fn test_block_on_with_future_chain() {
        let runtime = Runtime::new();
        let result = runtime.block_on(async {
            let x = async { 10 }.await;
            let y = async { 20 }.await;
            x + y
        });
        assert_eq!(result, 30);
    }

    #[test]
    fn test_executor_tick() {
        let mut executor = Executor::new();
        
        // No tasks initially
        assert_eq!(executor.task_count(), 0);
        assert!(!executor.tick()); // No progress possible
        
        // Add a task
        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();
        
        let _handle = executor.spawn(async move {
            completed_clone.store(true, Ordering::SeqCst);
            42
        });
        
        assert_eq!(executor.task_count(), 1);
        
        // Tick should run the task
        assert!(executor.tick());
        assert!(completed.load(Ordering::SeqCst));
        assert_eq!(executor.task_count(), 0); // Task completed
    }

    #[test]
    fn test_executor_run() {
        let mut executor = Executor::new();
        let counter = Arc::new(AtomicU32::new(0));
        
        // Spawn multiple tasks
        for i in 0..5 {
            let counter_clone = counter.clone();
            executor.spawn(async move {
                counter_clone.fetch_add(i, Ordering::SeqCst);
            });
        }
        
        assert_eq!(executor.task_count(), 5);
        
        // Run all tasks to completion
        executor.run();
        
        assert_eq!(executor.task_count(), 0);
        assert_eq!(counter.load(Ordering::SeqCst), 0 + 1 + 2 + 3 + 4);
    }

    #[test]
    fn test_join_handle_basic() {
        let mut executor = Executor::new();
        let handle = executor.spawn(async { "hello world" });
        
        // Initially not finished
        assert!(!handle.is_finished());
        
        // Run the executor
        executor.run();
        
        // Now should be finished
        assert!(handle.is_finished());
    }

    #[test]
    fn test_multiple_tasks() {
        let mut executor = Executor::new();
        let results = Arc::new(std::sync::Mutex::new(Vec::new()));
        
        // Spawn tasks that complete in different "rounds"
        for i in 0..3 {
            let results = results.clone();
            executor.spawn(async move {
                results.lock().unwrap().push(i);
            });
        }
        
        executor.run();
        
        let final_results = results.lock().unwrap();
        assert_eq!(final_results.len(), 3);
        // Results might be in any order due to task scheduling
        assert!(final_results.contains(&0));
        assert!(final_results.contains(&1));
"""        assert!(final_results.contains(&2));
    }

    #[test]
    fn test_task_panic_isolated() {
        let mut executor = Executor::new();
        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = flag.clone();

        executor.spawn(async {
            panic!("boom");
        });

        executor.spawn(async move {
            flag_clone.store(true, Ordering::SeqCst);
        });

        // This will panic if the panic is not caught
        executor.run();

        assert!(flag.load(Ordering::SeqCst), "Task B should have run");
    }

    #[test]
    fn test_multiple_panics() {
        let mut executor = Executor::new();
        let completed_count = Arc::new(AtomicU32::new(0));

        for i in 0..10 {
            let completed_count_clone = completed_count.clone();
            if i % 2 == 0 {
                executor.spawn(async move {
                    panic!("boom {}", i);
                });
            } else {
                executor.spawn(async move {
                    completed_count_clone.fetch_add(1, Ordering::SeqCst);
                });
            }
        }

        executor.run();

        assert_eq!(completed_count.load(Ordering::SeqCst), 5, "All non-panicking tasks should complete");
    }
}""
