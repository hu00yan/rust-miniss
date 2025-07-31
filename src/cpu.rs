use std::sync::atomic::{AtomicU64, Ordering};

/// Per-CPU executor implementation
/// 
/// This module provides CPU-local executors that run on dedicated threads,
/// implementing the shared-nothing architecture principle.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread::JoinHandle as ThreadJoinHandle;
use std::time::Duration;
use crossbeam_queue::SegQueue;
use crossbeam_channel::{Receiver, Sender};

use crate::task::{Task, JoinHandle};
use crate::waker::{MinissWaker, TaskId};

/// Global atomic counter for generating unique task IDs across all CPUs
static GLOBAL_TASK_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generate a globally unique task ID
/// 
/// This function ensures that task IDs are unique across all CPUs,
/// preventing collisions during cross-CPU task submission.
fn generate_global_task_id() -> TaskId {
    TaskId(GLOBAL_TASK_ID_COUNTER.fetch_add(1, Ordering::SeqCst))
}

/// Represents a single CPU core with its own executor
pub struct Cpu {
    /// CPU core ID (0-based)
    pub id: usize,
    /// Local task queue for this CPU
    task_queue: HashMap<TaskId, Task>,
    /// Ready queue for tasks that can be executed
    ready_queue: Arc<SegQueue<TaskId>>,
    /// Cross-CPU message receiver
    message_receiver: Receiver<CrossCpuMessage>,
    /// Next task ID for this CPU
    next_task_id: AtomicU64,
    /// Whether this CPU should keep running
    running: bool,
}

/// Messages that can be sent between CPUs
pub enum CrossCpuMessage {
    /// Submit a new task to this CPU
    SubmitTask {
        task_id: TaskId,
        task: Box<dyn Future<Output = ()> + Send>,
    },
    /// Signal this CPU to shutdown
    Shutdown,
    /// Ping message for testing cross-CPU communication
    Ping { reply_to: usize },
    /// Cancel a specific task
    CancelTask(TaskId),
}

impl std::fmt::Debug for CrossCpuMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CrossCpuMessage::SubmitTask { task_id, .. } => {
                f.debug_struct("SubmitTask")
                    .field("task_id", task_id)
                    .field("task", &"<Future>")
                    .finish()
            }
            CrossCpuMessage::Shutdown => f.debug_struct("Shutdown").finish(),
            CrossCpuMessage::Ping { reply_to } => {
                f.debug_struct("Ping")
                    .field("reply_to", reply_to)
                    .finish()
            }
            CrossCpuMessage::CancelTask(task_id) => {
                f.debug_struct("CancelTask")
                    .field("task_id", task_id)
                    .finish()
            }
        }
    }
}

/// Handle for communicating with a specific CPU
#[derive(Debug)]
pub struct CpuHandle {
    pub cpu_id: usize,
    sender: Sender<CrossCpuMessage>,
    thread_handle: Option<ThreadJoinHandle<()>>,
}

impl Cpu {
    /// Create a new CPU executor
    pub fn new(id: usize, message_receiver: Receiver<CrossCpuMessage>) -> Self {
        Self {
            id,
            task_queue: HashMap::new(),
            ready_queue: Arc::new(SegQueue::new()),
            message_receiver,
            next_task_id: AtomicU64::new((id as u64) << 32), // High bits = CPU ID
            running: true,
        }
    }

    /// Get the next unique task ID for this CPU
fn next_task_id(&self) -> TaskId {
        TaskId(self.next_task_id.fetch_add(1, Ordering::SeqCst))
    }

    /// Spawn a task on this CPU
    pub fn spawn<F, T>(&mut self, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let task_id = self.next_task_id();
        let (result_future, promise) = crate::future::Future::new();

        // Wrap the user's future to complete our promise when done
        // Panics will be caught at the polling level
        let wrapped_future = async move {
            let result = future.await;
            promise.complete(Ok(result));
        };

        // Create the task
        let task = Task::new(task_id, wrapped_future);
        
        // Add to our task list and ready queue
        self.task_queue.insert(task_id, task);
        self.ready_queue.push(task_id);

        JoinHandle::new(task_id, result_future)
    }

    /// Process cross-CPU messages
    fn process_messages(&mut self) {
        while let Ok(message) = self.message_receiver.try_recv() {
            self.handle_message(message);
        }
    }

    /// Handle a single cross-CPU message
    fn handle_message(&mut self, message: CrossCpuMessage) {
        match message {
            CrossCpuMessage::SubmitTask { task_id, task } => {
                tracing::debug!("CPU {} received task {:?}", self.id, task_id);
                
                let pinned_task = unsafe { Pin::new_unchecked(task) };
                let task = Task::from_pinned(task_id, pinned_task);
                self.task_queue.insert(task_id, task);
                self.ready_queue.push(task_id);
            }
            CrossCpuMessage::Shutdown => {
                tracing::info!("CPU {} received shutdown signal", self.id);
                self.running = false;
            }
            CrossCpuMessage::Ping { reply_to } => {
                tracing::debug!("CPU {} received ping from CPU {}", self.id, reply_to);
                // For now, just log it. In a full implementation, we'd reply back
            }
            CrossCpuMessage::CancelTask(task_id) => {
                tracing::debug!("CPU {} cancelling task {:?}", self.id, task_id);
                self.task_queue.remove(&task_id);
            }
        }
    }

    /// Run one iteration of the event loop
    pub fn tick(&mut self) -> bool {
        let mut made_progress = false;

        // First, process any cross-CPU messages
        self.process_messages();

        // Then, run ready tasks
        while let Some(task_id) = self.ready_queue.pop() {
            if let Some(mut task) = self.task_queue.remove(&task_id) {
                // Create a waker for this task
                let waker = MinissWaker::new(task_id, self.ready_queue.clone());
                let mut context = Context::from_waker(&waker);

                // Poll the task
                match task.poll(&mut context) {
                    Poll::Ready(()) => {
                        // Task completed, don't put it back
                        made_progress = true;
                        tracing::trace!("CPU {} completed task {:?}", self.id, task_id);
                        
                        // Note: In a full implementation, we would need to notify the runtime
                        // to remove this task from the task_cpu_map. For now, we rely on
                        // the runtime's cancel operation to handle cleanup when tasks are not found.
                    }
                    Poll::Pending => {
                        // Task is still pending, put it back
                        self.task_queue.insert(task_id, task);
                        made_progress = true;
                    }
                }
            }
        }

        made_progress
    }

    /// Main event loop for this CPU
    pub fn run(&mut self) {
        tracing::info!("CPU {} starting event loop", self.id);
        
        // Set CPU affinity if supported
        self.set_cpu_affinity();

        use crossbeam_channel::RecvTimeoutError;
        while self.running {
            // Execute any ready tasks and process any buffered messages.
            self.tick();

            // If the ready queue is empty, wait for a new message.
            if self.ready_queue.is_empty() {
                match self.message_receiver.recv_timeout(Duration::from_millis(crate::config::CPU_THREAD_TIMEOUT_MS)) {
                    Ok(msg) => {
                        self.handle_message(msg);
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        // Loop to check self.running
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        self.running = false;
                    }
                }
            }
        }

        tracing::info!("CPU {} shutting down", self.id);
    }

    /// Set CPU affinity for this thread (Linux only)
    #[cfg(target_os = "linux")]
    fn set_cpu_affinity(&self) {
        use nix::sched::{sched_setaffinity, CpuSet};
        use nix::unistd::Pid;

        let mut cpu_set = CpuSet::new();
        cpu_set.set(self.id).unwrap();

        if let Err(e) = sched_setaffinity(Pid::from_raw(0), &cpu_set) {
            tracing::warn!("Failed to set CPU affinity for CPU {}: {}", self.id, e);
        } else {
            tracing::debug!("Set CPU affinity for CPU {} to core {}", self.id, self.id);
        }
    }

    /// Get the number of active tasks on this CPU
    pub fn task_count(&self) -> usize {
        self.task_queue.len()
    }

    /// No-op for non-Linux platforms
    #[cfg(not(target_os = "linux"))]
    fn set_cpu_affinity(&self) {
        // Not supported on this platform
    }

    /// Check if this CPU is still running
    pub fn is_running(&self) -> bool {
        self.running
    }
}

impl CpuHandle {
    /// Create a new CPU handle
    pub fn new(cpu_id: usize) -> (Self, Receiver<CrossCpuMessage>) {
        // Use bounded channels for back-pressure control
        // Bounded channels prevent uncontrolled memory growth under high load.
        // The capacity is configurable via config::CROSS_CPU_CHANNEL_CAPACITY
        // for tuning based on workload requirements.
        let (sender, receiver) = crossbeam_channel::bounded(crate::config::CROSS_CPU_CHANNEL_CAPACITY);
        
        let handle = Self {
            cpu_id,
            sender,
            thread_handle: None,
        };
        
        (handle, receiver)
    }

    /// Submit a task to this CPU from another CPU
    /// Returns the generated task ID for tracking purposes
    pub fn submit_task<F>(&self, task: F) -> Result<TaskId, crossbeam_channel::SendError<CrossCpuMessage>>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        // Use global atomic counter for robust task ID generation (Issue #6)
        // This ensures unique task IDs across all CPUs without collisions.
        let task_id = generate_global_task_id();
        let message = CrossCpuMessage::SubmitTask {
            task_id,
            task: Box::new(task),
        };
        
        self.sender.send(message).map(|_| task_id)
    }

    /// Send a shutdown signal to this CPU
    pub fn shutdown(&self) -> Result<(), crossbeam_channel::SendError<CrossCpuMessage>> {
        self.sender.send(CrossCpuMessage::Shutdown)
    }

    /// Ping this CPU (for testing)
    pub fn ping(&self, from_cpu: usize) -> Result<(), crossbeam_channel::SendError<CrossCpuMessage>> {
        self.sender.send(CrossCpuMessage::Ping { reply_to: from_cpu })
    }

    /// Cancel a specific task on this CPU
    pub fn cancel_task(&self, task_id: TaskId) -> Result<(), crossbeam_channel::SendError<CrossCpuMessage>> {
        self.sender.send(CrossCpuMessage::CancelTask(task_id))
    }

    /// Set the thread handle (called after spawning the CPU thread)
    pub fn set_thread_handle(&mut self, handle: ThreadJoinHandle<()>) {
        self.thread_handle = Some(handle);
    }

    /// Wait for the CPU thread to finish
    pub fn join(self) -> std::thread::Result<()> {
        if let Some(handle) = self.thread_handle {
            handle.join()
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_cpu_creation() {
        let (_sender, receiver) = crossbeam_channel::unbounded();
        let cpu = Cpu::new(0, receiver);
        assert_eq!(cpu.id, 0);
        assert_eq!(cpu.task_count(), 0);
        assert!(cpu.is_running());
    }

    #[test]
    fn test_cpu_spawn_task() {
        let (_sender, receiver) = crossbeam_channel::unbounded();
        let mut cpu = Cpu::new(0, receiver);
        
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        
        let _handle = cpu.spawn(async move {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });
        
        assert_eq!(cpu.task_count(), 1);
        
        // Run one tick to execute the task
        assert!(cpu.tick());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(cpu.task_count(), 0); // Task completed
    }

    #[test]
    fn test_cross_cpu_message() {
        let (handle, receiver) = CpuHandle::new(1);
        let mut cpu = Cpu::new(1, receiver);
        
        // Send a ping message
        handle.ping(0).unwrap();
        
        // Process messages
        cpu.process_messages();
        
        // The message should have been processed (we just log it for now)
        assert!(cpu.is_running());
    }

    #[test]
    fn test_cpu_shutdown() {
        let (handle, receiver) = CpuHandle::new(1);
        let mut cpu = Cpu::new(1, receiver);
        
        assert!(cpu.is_running());
        
        // Send shutdown signal
        handle.shutdown().unwrap();
        
        // Process messages
        cpu.process_messages();
        
        assert!(!cpu.is_running());
    }

    #[test]
    fn test_task_id_uniqueness() {
        let (_sender, receiver) = crossbeam_channel::unbounded();
        let cpu = Cpu::new(5, receiver); // CPU 5
        
        let id1 = cpu.next_task_id();
        let id2 = cpu.next_task_id();
        
        assert_ne!(id1, id2);
        
        // Check that CPU ID is encoded in high bits
        assert_eq!(id1.0 >> 32, 5);
        assert_eq!(id2.0 >> 32, 5);
    }

    #[test]
    fn test_global_task_id_generation() {
        let id1 = generate_global_task_id();
        let id2 = generate_global_task_id();
        let id3 = generate_global_task_id();
        
        // All IDs should be unique
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
        
        // IDs should be sequential
        assert_eq!(id2.0, id1.0 + 1);
        assert_eq!(id3.0, id2.0 + 1);
    }
}
