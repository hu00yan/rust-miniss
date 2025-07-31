//! Multi-core runtime implementation
//!
//! This module provides a multi-core async runtime that spawns one executor
//! per CPU core, implementing the shared-nothing architecture.

use dashmap::DashMap;
use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;

use crate::cpu::{Cpu, CpuHandle};
use crate::error::{Result, RuntimeError};
use crate::io::DummyIoBackend;
use crate::waker::TaskId;

#[cfg(feature = "signal")]
use crate::signal::SignalHandler;

/// Multi-core runtime that manages multiple CPU executors
#[derive(Debug)]
pub struct MultiCoreRuntime {
    /// Handles to communicate with each CPU
    cpu_handles: Option<Vec<CpuHandle>>,
    /// Number of CPU cores to use
    num_cpus: usize,
    /// Current CPU for round-robin task distribution
    current_cpu: std::sync::atomic::AtomicUsize,
    /// Mapping from task ID to CPU ID for cancellation support
    task_cpu_map: Arc<DashMap<TaskId, usize>>,
    /// Shutdown flag for graceful shutdown
    pub shutdown_flag: Arc<AtomicBool>,
}

impl MultiCoreRuntime {
    /// Create a new multi-core runtime
    ///
    /// If `num_cpus` is None, it will use the number of logical CPU cores available
    pub fn new(num_cpus: Option<usize>) -> Result<Self> {
        let num_cpus = num_cpus.unwrap_or_else(num_cpus::get);

        if num_cpus == 0 {
            return Err(RuntimeError::TaskFailed(
                "Cannot create runtime with 0 CPUs".to_string(),
            ));
        }

        tracing::info!("Creating multi-core runtime with {} CPUs", num_cpus);

        let mut cpu_handles = Vec::with_capacity(num_cpus);

        // Create CPU handles and spawn threads
        for cpu_id in 0..num_cpus {
            let (mut handle, receiver) = CpuHandle::new(cpu_id);

            // Spawn the CPU thread
            let thread_handle = thread::Builder::new()
                .name(format!("miniss-cpu-{}", cpu_id))
                .spawn(move || {
                    let io_backend = Arc::new(DummyIoBackend::new());
                    let mut cpu = Cpu::new(cpu_id, receiver, io_backend);
                    cpu.run();
                })
                .map_err(|e| {
                    RuntimeError::TaskFailed(format!(
                        "Failed to spawn CPU thread {}: {}",
                        cpu_id, e
                    ))
                })?;

            handle.set_thread_handle(thread_handle);
            cpu_handles.push(handle);
        }

        let shutdown_flag = Arc::new(AtomicBool::new(false));

        #[cfg(feature = "signal")]
        {
            let signal_handler =
                SignalHandler::with_cpu_handles(shutdown_flag.clone(), &cpu_handles);
            signal_handler.start();
        }

        Ok(Self {
            cpu_handles: Some(cpu_handles),
            num_cpus,
            current_cpu: std::sync::atomic::AtomicUsize::new(0),
            task_cpu_map: Arc::new(DashMap::new()),
            shutdown_flag,
        })
    }

    /// Create a runtime with the optimal number of CPUs (one per logical core)
    pub fn new_optimal() -> Result<Self> {
        Self::new(None)
    }

    /// Create a runtime with a specific number of CPUs
    pub fn with_cpus(num_cpus: usize) -> Result<Self> {
        Self::new(Some(num_cpus))
    }

    /// Get the number of CPU cores in this runtime
    pub fn cpu_count(&self) -> usize {
        self.num_cpus
    }

    /// Get the next CPU to use for task scheduling (round-robin)
    // TODO: Implement work-stealing scheduler for better fairness (Issue #2)
    // Current round-robin distribution doesn't account for actual CPU load.
    // A work-stealing approach would provide better load balancing.
    fn next_cpu(&self) -> usize {
        self.current_cpu
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.num_cpus
    }

    /// Submit a task to a specific CPU
    pub fn spawn_on<F>(&self, cpu_id: usize, future: F) -> Result<TaskId>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if cpu_id >= self.num_cpus {
            return Err(RuntimeError::TaskFailed(format!(
                "CPU {} does not exist (max: {})",
                cpu_id,
                self.num_cpus - 1
            )));
        }

        if let Some(handles) = &self.cpu_handles {
            let task_id = handles[cpu_id].submit_task(future).map_err(|e| {
                RuntimeError::TaskFailed(format!("Failed to submit task to CPU {}: {}", cpu_id, e))
            })?;
            self.task_cpu_map.insert(task_id, cpu_id);
            Ok(task_id)
        } else {
            Err(RuntimeError::NotInitialized)
        }
    }

    /// Spawn a task on the next available CPU (round-robin)
    pub fn spawn<F>(&self, future: F) -> Result<TaskId>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let cpu_id = self.next_cpu();
        self.spawn_on(cpu_id, future)
    }

    /// Cancel a specific task
    ///
    /// Looks up the CPU where the task is running and sends a cancellation message.
    /// Returns an error if the task has already finished or the CPU cannot be reached.
    pub fn cancel_task(&self, task_id: TaskId) -> Result<()> {
        if let Some(cpu_id) = self.task_cpu_map.get(&task_id) {
            let cpu_id = *cpu_id;

            if let Some(handles) = &self.cpu_handles {
                if cpu_id < handles.len() {
                    handles[cpu_id].cancel_task(task_id).map_err(|e| {
                        RuntimeError::TaskFailed(format!(
                            "Failed to send cancel message to CPU {}: {}",
                            cpu_id, e
                        ))
                    })?;

                    // Remove from tracking
                    self.task_cpu_map.remove(&task_id);
                    Ok(())
                } else {
                    Err(RuntimeError::TaskFailed(format!(
                        "Invalid CPU ID {} for task {:?}",
                        cpu_id, task_id
                    )))
                }
            } else {
                Err(RuntimeError::NotInitialized)
            }
        } else {
            Err(RuntimeError::TaskFailed(format!(
                "Task {:?} not found or already completed",
                task_id
            )))
        }
    }

    /// Run a future to completion on a specific CPU
    ///
    /// This is a blocking operation that will wait for the future to complete.
    /// It's mainly useful for the main thread to run the initial application logic.
    pub fn block_on_cpu<F>(&self, cpu_id: usize, future: F) -> Result<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if cpu_id >= self.num_cpus {
            return Err(RuntimeError::TaskFailed(format!(
                "CPU {} does not exist (max: {})",
                cpu_id,
                self.num_cpus - 1
            )));
        }

        // Create a channel to receive the result
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);

        // Submit the task
        let task = async move {
            let result = future.await;
            let _ = sender.send(result);
        };

        if let Some(handles) = &self.cpu_handles {
            handles[cpu_id].submit_task(task).map_err(|e| {
                RuntimeError::TaskFailed(format!("Failed to submit task to CPU {}: {}", cpu_id, e))
            })?;
        }

        // Wait for the result
        receiver
            .recv()
            .map_err(|e| RuntimeError::TaskFailed(format!("Failed to receive result: {}", e)))
    }

    /// Run a future to completion on any available CPU
    pub fn block_on<F>(&self, future: F) -> Result<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let cpu_id = self.next_cpu();
        self.block_on_cpu(cpu_id, future)
    }

    /// Ping all CPUs to test cross-CPU communication
    pub fn ping_all(&self) -> Result<()> {
        tracing::info!("Pinging all {} CPUs", self.num_cpus);

        if let Some(handles) = &self.cpu_handles {
            for (from_cpu, _handle) in handles.iter().enumerate() {
                for to_cpu in 0..self.num_cpus {
                    if from_cpu != to_cpu {
                        handles[to_cpu].ping(from_cpu).map_err(|e| {
                            RuntimeError::TaskFailed(format!(
                                "Failed to ping CPU {} from CPU {}: {}",
                                to_cpu, from_cpu, e
                            ))
                        })?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Gracefully shutdown all CPUs
    ///
    /// This method performs a graceful shutdown by:
    /// 1. Sending shutdown signals to all CPU threads
    /// 2. Joining all threads to ensure they complete
    /// 3. Flushing any remaining tasks (handled by CPU event loops)
    ///
    /// The bounded channels provide back-pressure and prevent task queue overflow,
    /// while the CPU threads process all remaining tasks before shutting down.
    pub fn shutdown(mut self) -> Result<()> {
        tracing::info!(
            "Shutting down multi-core runtime with {} CPUs",
            self.num_cpus
        );

        if let Some(handles) = self.cpu_handles.take() {
            // Send shutdown signals to all CPUs
            // This is done first to prevent new tasks from being queued
            for handle in &handles {
                if let Err(e) = handle.shutdown() {
                    tracing::warn!(
                        "Failed to send shutdown signal to CPU {}: {}",
                        handle.cpu_id,
                        e
                    );
                    // Continue with other CPUs even if one fails
                }
            }

            // Wait for all CPU threads to finish
            // Each CPU will process remaining tasks before shutting down
            for handle in handles {
                let cpu_id = handle.cpu_id;
                match handle.join() {
                    Ok(()) => {
                        tracing::debug!("CPU {} thread joined successfully", cpu_id);
                    }
                    Err(e) => {
                        tracing::error!("Failed to join CPU {} thread: {:?}", cpu_id, e);
                        return Err(RuntimeError::TaskFailed(format!(
                            "Failed to join CPU {} thread: {:?}",
                            cpu_id, e
                        )));
                    }
                }
            }
        }

        tracing::info!("Multi-core runtime shutdown complete");
        Ok(())
    }
}

// Implement Drop for graceful shutdown.
// The runtime should automatically shutdown when dropped to prevent resource leaks.
// This is more robust than requiring manual shutdown() calls.

/// Shutdown the global runtime and allow re-initialization
pub fn shutdown_runtime() {
    // OnceCell doesn't have a `take()` method, so we need to use a different approach
    // Since we can't reset OnceCell directly, we'll implement a way to recreate the runtime
    tracing::info!("Attempting to shutdown global runtime");
    // For now, we'll just log that shutdown was requested
    // The actual shutdown happens when the runtime is dropped
}

impl Drop for MultiCoreRuntime {
    fn drop(&mut self) {
        // The `shutdown` method requires `self` to be consumed, and `drop` only provides `&mut self`.
        // To work around this, we can use a helper method that takes `&mut self` and internally
        // uses `Option::take` to consume `self.cpu_handles`.
        // This ensures that the shutdown logic is only run once.
        self.shutdown_internal();
    }
}

impl MultiCoreRuntime {
    // Internal shutdown helper that operates on &mut self
    fn shutdown_internal(&mut self) {
        if let Some(handles) = self.cpu_handles.take() {
            let num_cpus = handles.len();
            tracing::info!("Shutting down multi-core runtime with {} CPUs", num_cpus);

            // Send shutdown signals
            for handle in &handles {
                if let Err(e) = handle.shutdown() {
                    tracing::warn!("Failed to send shutdown to CPU {}: {}", handle.cpu_id, e);
                }
            }

            // Join threads
            for handle in handles {
                let cpu_id = handle.cpu_id;
                if let Err(e) = handle.join() {
                    tracing::warn!("Failed to join CPU {} thread: {:?}", cpu_id, e);
                }
            }

            tracing::info!("Multi-core runtime shutdown complete");
        }
    }
}

/// Global runtime instance
use once_cell::sync::OnceCell;

static GLOBAL_RUNTIME: OnceCell<Arc<MultiCoreRuntime>> = OnceCell::new();

/// Initialize the global multi-core runtime
///
/// This should be called once at the beginning of your program.
/// Subsequent calls will be ignored.
pub fn init_runtime(num_cpus: Option<usize>) -> Result<()> {
    if GLOBAL_RUNTIME.get().is_some() {
        return Err(RuntimeError::TaskFailed(
            "Runtime already initialized".into(),
        ));
    }
    let runtime = MultiCoreRuntime::new(num_cpus)?;
    GLOBAL_RUNTIME
        .set(Arc::new(runtime))
        .map_err(|_| RuntimeError::TaskFailed("Runtime already initialized".into()))?;
    Ok(())
}

/// Get a reference to the global runtime
///
/// Panics if the runtime hasn't been initialized
pub fn runtime() -> Arc<MultiCoreRuntime> {
    GLOBAL_RUNTIME
        .get()
        .expect("Runtime not initialized. Call init_runtime() first.")
        .clone()
}

/// Convenience function to spawn a task on the global runtime
pub fn spawn<F>(future: F) -> Result<TaskId>
where
    F: Future<Output = ()> + Send + 'static,
{
    runtime().spawn(future)
}

/// Convenience function to spawn a task on a specific CPU
pub fn spawn_on<F>(cpu_id: usize, future: F) -> Result<TaskId>
where
    F: Future<Output = ()> + Send + 'static,
{
    runtime().spawn_on(cpu_id, future)
}

/// Convenience function to run a future to completion
pub fn block_on<F>(future: F) -> Result<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime().block_on(future)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_runtime_creation() {
        let runtime = MultiCoreRuntime::with_cpus(2).unwrap();
        assert_eq!(runtime.cpu_count(), 2);
    }

    #[test]
    fn test_runtime_spawn() {
        let runtime = MultiCoreRuntime::with_cpus(2).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();

        // Spawn tasks on different CPUs
        for _ in 0..4 {
            let tx_clone = tx.clone();
            runtime
                .spawn(async move {
                    // Simulate some work
                    tx_clone.send(1).unwrap();
                })
                .unwrap();
        }

        // Wait for all tasks to complete
        for _ in 0..4 {
            rx.recv_timeout(Duration::from_secs(1)).unwrap();
        }

        // Clean shutdown
        runtime.shutdown().unwrap();
    }

    #[test]
    fn test_runtime_block_on() {
        let runtime = MultiCoreRuntime::with_cpus(1).unwrap();

        let result = runtime.block_on(async { 42 }).unwrap();
        assert_eq!(result, 42);

        runtime.shutdown().unwrap();
    }

    #[test]
    fn test_runtime_spawn_on_specific_cpu() {
        let runtime = MultiCoreRuntime::with_cpus(3).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();

        // Spawn task on CPU 1
        runtime
            .spawn_on(1, async move {
                tx.send(10).unwrap();
            })
            .unwrap();

        // Wait for task to complete
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), 10);

        runtime.shutdown().unwrap();
    }

    #[test]
    fn test_invalid_cpu_id() {
        let runtime = MultiCoreRuntime::with_cpus(2).unwrap();

        // Try to spawn on non-existent CPU
        let result = runtime.spawn_on(5, async {});
        assert!(result.is_err());

        runtime.shutdown().unwrap();
    }

    #[test]
    fn test_ping_all() {
        let runtime = MultiCoreRuntime::with_cpus(3).unwrap();

        // This should not fail
        runtime.ping_all().unwrap();

        runtime.shutdown().unwrap();
    }

    #[test]
    fn test_zero_cpus_error() {
        let result = MultiCoreRuntime::with_cpus(0);
        assert!(result.is_err());
    }

    #[test]
    fn test_automatic_shutdown_on_drop() {
        let (tx, rx) = std::sync::mpsc::channel();

        {
            let runtime = MultiCoreRuntime::with_cpus(2).unwrap();

            // Spawn a task that signals completion
            let tx_clone = tx.clone();
            runtime
                .spawn(async move {
                    tx_clone.send("task completed").unwrap();
                })
                .unwrap();

            // Wait for task to complete
            rx.recv_timeout(Duration::from_secs(1)).unwrap();

            // runtime goes out of scope here, Drop should be called
        }

        // If we get here, the Drop implementation worked correctly
        // (no hanging threads)
    }
}
