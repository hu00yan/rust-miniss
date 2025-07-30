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
use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::task::{Task, JoinHandle};
use crate::waker::{MinissWaker, TaskId};

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
        let wrapped_future = async move {
            let result = future.await;
            promise.complete(result);
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
        
        #[cfg(target_os = "linux")]
        self.set_cpu_affinity();

        use crossbeam_channel::RecvTimeoutError;
        while self.running {
            // Execute any ready tasks and process any buffered messages.
            self.tick();

            // If the ready queue is empty, wait for a new message.
            if self.ready_queue.is_empty() {
                match self.message_receiver.recv_timeout(Duration::from_millis(10)) {
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

    /// Check if this CPU is still running
    pub fn is_running(&self) -> bool {
        self.running
    }
}

impl CpuHandle {
    /// Create a new CPU handle
    pub fn new(cpu_id: usize) -> (Self, Receiver<CrossCpuMessage>) {
        let (sender, receiver) = unbounded();
        
        let handle = Self {
            cpu_id,
            sender,
            thread_handle: None,
        };
        
        (handle, receiver)
    }

    /// Submit a task to this CPU from another CPU
    pub fn submit_task<F>(&self, task: F) -> Result<(), crossbeam_channel::SendError<CrossCpuMessage>>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let task_id = TaskId(rand::random()); // In practice, we'd want a better ID scheme
        let message = CrossCpuMessage::SubmitTask {
            task_id,
            task: Box::new(task),
        };
        
        self.sender.send(message)
    }

    /// Send a shutdown signal to this CPU
    pub fn shutdown(&self) -> Result<(), crossbeam_channel::SendError<CrossCpuMessage>> {
        self.sender.send(CrossCpuMessage::Shutdown)
    }

    /// Ping this CPU (for testing)
    pub fn ping(&self, from_cpu: usize) -> Result<(), crossbeam_channel::SendError<CrossCpuMessage>> {
        self.sender.send(CrossCpuMessage::Ping { reply_to: from_cpu })
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
        let (_, receiver) = unbounded();
        let cpu = Cpu::new(0, receiver);
        assert_eq!(cpu.id, 0);
        assert_eq!(cpu.task_count(), 0);
        assert!(cpu.is_running());
    }

    #[test]
    fn test_cpu_spawn_task() {
        let (_, receiver) = unbounded();
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
        let (_, receiver) = unbounded();
        let cpu = Cpu::new(5, receiver); // CPU 5
        
        let id1 = cpu.next_task_id();
        let id2 = cpu.next_task_id();
        
        assert_ne!(id1, id2);
        
        // Check that CPU ID is encoded in high bits
        assert_eq!(id1.0 >> 32, 5);
        assert_eq!(id2.0 >> 32, 5);
    }
}
