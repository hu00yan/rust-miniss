//! Multi-core runtime implementation
//! 
//! This module provides a multi-core async runtime that spawns one executor
//! per CPU core, implementing the shared-nothing architecture.

use std::future::Future;
use std::sync::Arc;
use std::thread;

use crate::cpu::{Cpu, CpuHandle};
use crate::error::{Result, RuntimeError};

/// Multi-core runtime that manages multiple CPU executors
#[derive(Debug)]
pub struct MultiCoreRuntime {
    /// Handles to communicate with each CPU
    cpu_handles: Vec<CpuHandle>,
    /// Number of CPU cores to use
    num_cpus: usize,
    /// Current CPU for round-robin task distribution
    current_cpu: std::sync::atomic::AtomicUsize,
}

impl MultiCoreRuntime {
    /// Create a new multi-core runtime
    /// 
    /// If `num_cpus` is None, it will use the number of logical CPU cores available
    pub fn new(num_cpus: Option<usize>) -> Result<Self> {
        let num_cpus = num_cpus.unwrap_or_else(num_cpus::get);
        
        if num_cpus == 0 {
            return Err(RuntimeError::TaskFailed("Cannot create runtime with 0 CPUs".to_string()));
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
                    let mut cpu = Cpu::new(cpu_id, receiver);
                    cpu.run();
                })
                .map_err(|e| RuntimeError::TaskFailed(format!("Failed to spawn CPU thread {}: {}", cpu_id, e)))?;
            
            handle.set_thread_handle(thread_handle);
            cpu_handles.push(handle);
        }

        Ok(Self {
            cpu_handles,
            num_cpus,
            current_cpu: std::sync::atomic::AtomicUsize::new(0),
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
    fn next_cpu(&self) -> usize {
        self.current_cpu.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.num_cpus
    }

    /// Submit a task to a specific CPU
    pub fn spawn_on<F>(&self, cpu_id: usize, future: F) -> Result<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if cpu_id >= self.num_cpus {
            return Err(RuntimeError::TaskFailed(format!("CPU {} does not exist (max: {})", cpu_id, self.num_cpus - 1)));
        }

        self.cpu_handles[cpu_id]
            .submit_task(future)
            .map_err(|e| RuntimeError::TaskFailed(format!("Failed to submit task to CPU {}: {}", cpu_id, e)))?;

        Ok(())
    }

    /// Spawn a task on the next available CPU (round-robin)
    pub fn spawn<F>(&self, future: F) -> Result<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let cpu_id = self.next_cpu();
        self.spawn_on(cpu_id, future)
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
            return Err(RuntimeError::TaskFailed(format!("CPU {} does not exist (max: {})", cpu_id, self.num_cpus - 1)));
        }

        // Create a channel to receive the result
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);

        // Submit the task
        let task = async move {
            let result = future.await;
            let _ = sender.send(result);
        };

        self.cpu_handles[cpu_id]
            .submit_task(task)
            .map_err(|e| RuntimeError::TaskFailed(format!("Failed to submit task to CPU {}: {}", cpu_id, e)))?;

        // Wait for the result
        receiver.recv()
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
        
        for (from_cpu, _handle) in self.cpu_handles.iter().enumerate() {
            for to_cpu in 0..self.num_cpus {
                if from_cpu != to_cpu {
                    self.cpu_handles[to_cpu]
                        .ping(from_cpu)
                        .map_err(|e| RuntimeError::TaskFailed(format!("Failed to ping CPU {} from CPU {}: {}", to_cpu, from_cpu, e)))?;
                }
            }
        }
        
        Ok(())
    }

    /// Gracefully shutdown all CPUs
    pub fn shutdown(self) -> Result<()> {
        tracing::info!("Shutting down multi-core runtime");

        // Send shutdown signals to all CPUs
        for handle in &self.cpu_handles {
            handle.shutdown()
                .map_err(|e| RuntimeError::TaskFailed(format!("Failed to send shutdown to CPU {}: {}", handle.cpu_id, e)))?;
        }

        // Wait for all CPU threads to finish
        for handle in self.cpu_handles {
            let cpu_id = handle.cpu_id;
            handle.join()
                .map_err(|e| RuntimeError::TaskFailed(format!("Failed to join CPU {} thread: {:?}", cpu_id, e)))?;
        }

        tracing::info!("Multi-core runtime shutdown complete");
        Ok(())
    }
}

// Drop implementation removed to avoid conflicts with shutdown method
// The shutdown method should be called explicitly for proper cleanup

/// Global runtime instance
use once_cell::sync::OnceCell;

static GLOBAL_RUNTIME: OnceCell<Arc<MultiCoreRuntime>> = OnceCell::new();

/// Initialize the global multi-core runtime
/// 
/// This should be called once at the beginning of your program.
/// Subsequent calls will be ignored.
pub fn init_runtime(num_cpus: Option<usize>) -> Result<()> {
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
pub fn spawn<F>(future: F) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    runtime().spawn(future)
}

/// Convenience function to spawn a task on a specific CPU
pub fn spawn_on<F>(cpu_id: usize, future: F) -> Result<()>
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
            runtime.spawn(async move {
                // Simulate some work
                tx_clone.send(1).unwrap();
            }).unwrap();
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
        runtime.spawn_on(1, async move {
            tx.send(10).unwrap();
        }).unwrap();

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
}
