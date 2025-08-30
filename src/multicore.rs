//! Pure Thread-Per-Core Runtime - Seastar/Glommio Inspired
//!
//! Implements shared-nothing architecture where each CPU core runs completely
//! independently with its own io_uring instance, task queue, and timer wheel.
//!
//! ## Core Principles
//! 1. **Shared Nothing**: Zero shared mutable state between cores
//! 2. **Message Passing**: All inter-core communication via lock-free queues
//! 3. **CPU Affinity**: Each core bound to physical CPU for optimal cache locality
//! 4. **Independent IO**: Each core has its own io_uring/epoll instance

use crossbeam_queue::SegQueue;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use crate::error::{Result, RuntimeError};
use crate::io::{CompletionKind, IoError, IoProvider, IoToken, Op};
use crate::task::Task;
use crate::timer::TimerWheel;
use crate::waker::TaskId;

/// Type alias for the IO backend completion type
type IoCompletion = (IoToken, Op, std::result::Result<CompletionKind, IoError>);

/// Global task ID generator
static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

/// Generate unique task ID
fn next_task_id() -> TaskId {
    TaskId(NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed))
}

/// Message types for inter-core communication
pub enum CoreMessage {
    /// Execute a task on this core
    Task {
        id: TaskId,
        future: Pin<Box<dyn Future<Output = ()> + Send>>,
    },
    /// Ping from another core (load balancing)
    Ping { from_core: usize },
    /// Shutdown signal
    Shutdown,
    /// Cancel a task
    CancelTask(TaskId),
}

impl std::fmt::Debug for CoreMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Task { id, .. } => f.debug_struct("Task").field("id", id).finish(),
            Self::Ping { from_core } => f
                .debug_struct("Ping")
                .field("from_core", from_core)
                .finish(),
            Self::Shutdown => write!(f, "Shutdown"),
            Self::CancelTask(task_id) => f
                .debug_struct("CancelTask")
                .field("task_id", task_id)
                .finish(),
        }
    }
}

/// Individual CPU core executor - completely independent
pub struct CpuCore {
    /// Core ID (matches physical CPU)
    id: usize,
    /// Local task queue - no synchronization needed
    task_queue: VecDeque<Task>,
    /// Message inbox from other cores
    message_inbox: Arc<SegQueue<CoreMessage>>,
    /// Local timer wheel
    #[allow(dead_code)]
    timer_wheel: TimerWheel,
    /// IO backend (io_uring/epoll/kqueue)
    io_backend: Box<dyn IoProvider<Completion = IoCompletion>>,
    /// Shutdown flag
    shutdown: Arc<AtomicBool>,
    /// Task counter for this core
    local_task_count: usize,
}

impl CpuCore {
    /// Create new CPU core
    fn new(
        id: usize,
        io_backend: Box<dyn IoProvider<Completion = IoCompletion>>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            id,
            task_queue: VecDeque::new(),
            message_inbox: Arc::new(SegQueue::new()),
            timer_wheel: TimerWheel::new(1024, 1),
            io_backend,
            shutdown,
            local_task_count: 0,
        }
    }

    /// Main event loop - shared-nothing execution
    fn run(&mut self) -> Result<()> {
        // Bind to CPU core for optimal cache locality
        self.bind_to_cpu()?;

        tracing::info!("CPU core {} started", self.id);

        while !self.shutdown.load(Ordering::Relaxed) {
            let mut work_done = false;

            // 1. Process inter-core messages
            work_done |= self.process_messages();

            // 2. Execute local tasks
            work_done |= self.execute_tasks();

            // 3. Process IO completions
            work_done |= self.process_io();

            // 4. Process timers
            work_done |= self.process_timers();

            // Yield CPU if no work was done
            if !work_done {
                thread::yield_now();
            }
        }

        tracing::info!("CPU core {} shutting down", self.id);
        Ok(())
    }

    /// Bind thread to CPU core (Linux-specific)
    #[cfg(target_os = "linux")]
    fn bind_to_cpu(&self) -> Result<()> {
        use libc::{cpu_set_t, sched_setaffinity, CPU_SET, CPU_ZERO};
        use std::mem;

        unsafe {
            let mut cpuset: cpu_set_t = mem::zeroed();
            CPU_ZERO(&mut cpuset);
            CPU_SET(self.id, &mut cpuset);

            let result = sched_setaffinity(0, mem::size_of::<cpu_set_t>(), &cpuset);
            if result != 0 {
                tracing::warn!("Failed to bind CPU core {}: {}", self.id, result);
            } else {
                tracing::debug!("CPU core {} bound to physical CPU", self.id);
            }
        }

        Ok(())
    }

    /// Fallback for non-Linux systems
    #[cfg(not(target_os = "linux"))]
    fn bind_to_cpu(&self) -> Result<()> {
        tracing::debug!("CPU binding not supported on this platform");
        Ok(())
    }

    /// Process messages from other cores
    fn process_messages(&mut self) -> bool {
        let mut processed = 0;

        // Process up to 32 messages per iteration to avoid starvation
        while processed < 32 {
            match self.message_inbox.pop() {
                Some(CoreMessage::Task { id, future }) => {
                    let task = Task::from_pinned(id, future);
                    self.task_queue.push_back(task);
                    self.local_task_count += 1;
                    processed += 1;
                }
                Some(CoreMessage::Ping { from_core }) => {
                    tracing::trace!("Received ping from core {}", from_core);
                    processed += 1;
                }
                Some(CoreMessage::Shutdown) => {
                    self.shutdown.store(true, Ordering::Relaxed);
                    processed += 1;
                    break;
                }
                Some(CoreMessage::CancelTask(task_id)) => {
                    // Remove task from queue if it exists
                    self.task_queue.retain(|task| task.id() != task_id);
                    tracing::trace!("Cancelled task {:?}", task_id);
                    processed += 1;
                }
                None => break,
            }
        }

        processed > 0
    }

    /// Execute local tasks
    fn execute_tasks(&mut self) -> bool {
        let mut executed = 0;

        // Execute up to 16 tasks per iteration
        while executed < 16 && !self.task_queue.is_empty() {
            if let Some(mut task) = self.task_queue.pop_front() {
                // Create waker for this task
                let waker = crate::waker::dummy_waker();
                let mut context = std::task::Context::from_waker(&waker);

                match task.poll(&mut context) {
                    std::task::Poll::Ready(()) => {
                        // Task completed
                        self.local_task_count -= 1;
                        tracing::trace!("Task {:?} completed on core {}", task.id(), self.id);
                    }
                    std::task::Poll::Pending => {
                        // Task not ready, put it back
                        self.task_queue.push_back(task);
                    }
                }
                executed += 1;
            }
        }

        executed > 0
    }

    /// Process IO completions
    fn process_io(&mut self) -> bool {
        // Create a dummy waker for polling
        let waker = crate::waker::dummy_waker();
        let mut context = std::task::Context::from_waker(&waker);

        // Poll IO backend for completions
        match self.io_backend.poll_complete(&mut context) {
            std::task::Poll::Ready(completions) => !completions.is_empty(),
            std::task::Poll::Pending => false,
        }
    }

    /// Process timer events
    fn process_timers(&mut self) -> bool {
        // Simple timer processing - check if any timers need processing
        // For now, just return false as we don't have expired timer processing
        false
    }
}

/// Runtime state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum RuntimeState {
    Initializing = 0,
    Running = 1,
    ShuttingDown = 2,
    Terminated = 3,
}

/// Multi-core runtime coordinator - minimal shared state
pub struct MultiCoreRuntime {
    /// Number of CPU cores
    num_cores: usize,
    /// Message senders for each core
    core_senders: Vec<Arc<SegQueue<CoreMessage>>>,
    /// Thread join handles
    join_handles: Vec<thread::JoinHandle<Result<()>>>,
    /// Runtime state
    state: AtomicU8,
    /// Next core for round-robin task distribution
    next_core: AtomicUsize,
}

impl std::fmt::Debug for MultiCoreRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.load(Ordering::Acquire);
        f.debug_struct("MultiCoreRuntime")
            .field("num_cores", &self.num_cores)
            .field("state", &state)
            .finish()
    }
}

impl MultiCoreRuntime {
    /// Create new multi-core runtime with specified number of CPU cores
    pub fn with_cpus(num_cores: usize) -> Result<Arc<Self>> {
        Self::new(Some(num_cores))
    }

    /// Create new multi-core runtime with optimal number of CPU cores
    pub fn new_optimal() -> Result<Arc<Self>> {
        Self::new(None)
    }

    /// Create new multi-core runtime
    pub fn new(num_cores: Option<usize>) -> Result<Arc<Self>> {
        let num_cores = num_cores.unwrap_or_else(num_cpus::get);

        if num_cores == 0 {
            return Err(RuntimeError::TaskFailed(
                "Cannot create runtime with 0 CPUs".to_string(),
            ));
        }

        tracing::info!("Creating thread-per-core runtime with {} cores", num_cores);

        let mut core_senders = Vec::with_capacity(num_cores);
        let mut join_handles = Vec::with_capacity(num_cores);

        // Create cores and start threads
        for core_id in 0..num_cores {
            // Create IO backend for this core
            let io_backend = Self::create_io_backend(core_id)?;

            // Create core with shutdown flag
            let core_shutdown = Arc::new(AtomicBool::new(false));
            let mut core = CpuCore::new(core_id, io_backend, core_shutdown.clone());

            // Store reference to core's message queue
            let sender = Arc::new(SegQueue::new());
            core_senders.push(sender.clone());
            core.message_inbox = sender;

            // Spawn thread for this core
            let handle = thread::Builder::new()
                .name(format!("miniss-core-{}", core_id))
                .spawn(move || {
                    let result = core.run();
                    // Set shutdown flag when core exits
                    core_shutdown.store(true, Ordering::Relaxed);
                    result
                })
                .map_err(|e| {
                    RuntimeError::TaskFailed(format!("Failed to spawn core thread: {}", e))
                })?;

            join_handles.push(handle);
        }

        let runtime = Arc::new(Self {
            num_cores,
            core_senders,
            join_handles,
            state: AtomicU8::new(RuntimeState::Initializing as u8),
            next_core: AtomicUsize::new(0),
        });

        // Set state to running after successful initialization
        runtime
            .state
            .store(RuntimeState::Running as u8, Ordering::Release);

        Ok(runtime)
    }

    /// Create IO backend for a specific core
    fn create_io_backend(core_id: usize) -> Result<Box<dyn IoProvider<Completion = IoCompletion>>> {
        #[cfg(all(target_os = "linux", io_backend = "io_uring"))]
        {
            match crate::io::uring::UringBackend::new(1024) {
                Ok(uring) => {
                    tracing::debug!("Core {} using io_uring backend", core_id);
                    return Ok(Box::new(uring));
                }
                Err(e) => {
                    tracing::warn!(
                        "Core {} failed to create io_uring: {}, falling back",
                        core_id,
                        e
                    );
                }
            }
        }

        #[cfg(target_os = "macos")]
        {
            match crate::io::kqueue::KqueueBackend::new() {
                Ok(kqueue) => {
                    tracing::debug!("Core {} using kqueue backend", core_id);
                    return Ok(Box::new(kqueue));
                }
                Err(e) => {
                    tracing::warn!("Core {} failed to create kqueue: {}", core_id, e);
                }
            }
        }

        #[cfg(all(target_os = "linux", io_backend = "epoll"))]
        {
            match crate::io::epoll::EpollBackend::new() {
                Ok(epoll) => {
                    tracing::debug!("Core {} using epoll backend", core_id);
                    return Ok(Box::new(epoll));
                }
                Err(e) => {
                    tracing::warn!("Core {} failed to create epoll: {}", core_id, e);
                }
            }
        }

        // All IO backends failed - this is a critical error
        tracing::error!("Core {}: All IO backends failed to initialize", core_id);
        Err(RuntimeError::IoFailed(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "No suitable IO backend available",
        )))
    }

    /// Spawn task on optimal core
    pub fn spawn<F>(&self, future: F) -> Result<TaskId>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        // Check if runtime is running
        let state = self.state.load(Ordering::Acquire);
        if state != RuntimeState::Running as u8 {
            return Err(RuntimeError::TaskFailed(
                "Runtime is not running".to_string(),
            ));
        }

        // Generate task ID
        let task_id = next_task_id();

        // Select core using round-robin
        let core_id = self.next_core.fetch_add(1, Ordering::Relaxed) % self.num_cores;

        self.spawn_on_core(core_id, task_id, future)
    }

    /// Spawn task on a specific core
    pub fn spawn_on<F>(&self, core_id: usize, future: F) -> Result<TaskId>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        // Check if runtime is running
        let state = self.state.load(Ordering::Acquire);
        if state != RuntimeState::Running as u8 {
            return Err(RuntimeError::TaskFailed(
                "Runtime is not running".to_string(),
            ));
        }

        // Validate core ID
        if core_id >= self.num_cores {
            return Err(RuntimeError::TaskFailed(format!(
                "Invalid core ID: {}",
                core_id
            )));
        }

        // Generate task ID
        let task_id = next_task_id();

        self.spawn_on_core(core_id, task_id, future)
    }

    /// Internal helper to spawn a task on a specific core
    fn spawn_on_core<F>(&self, core_id: usize, task_id: TaskId, future: F) -> Result<TaskId>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        // Send task to selected core
        let message = CoreMessage::Task {
            id: task_id,
            future: Box::pin(future),
        };

        self.core_senders[core_id].push(message);

        tracing::trace!("Task {:?} submitted to core {}", task_id, core_id);
        Ok(task_id)
    }

    /// Initiate graceful shutdown
    pub fn shutdown(&self) -> Result<()> {
        tracing::info!("Initiating runtime shutdown");

        // Try to transition to shutting down state
        let current_state = self.state.load(Ordering::Acquire);
        if current_state == RuntimeState::ShuttingDown as u8
            || current_state == RuntimeState::Terminated as u8
        {
            // Already shutting down or terminated
            return Ok(());
        }

        // Try to transition to shutting down state
        if self
            .state
            .compare_exchange(
                RuntimeState::Running as u8,
                RuntimeState::ShuttingDown as u8,
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_err()
        {
            // State changed by another thread, check again
            let current_state = self.state.load(Ordering::Acquire);
            if current_state == RuntimeState::ShuttingDown as u8
                || current_state == RuntimeState::Terminated as u8
            {
                return Ok(());
            }
            return Err(RuntimeError::TaskFailed(
                "Failed to initiate shutdown".to_string(),
            ));
        }

        // Send shutdown message to all cores
        for sender in &self.core_senders {
            sender.push(CoreMessage::Shutdown);
        }

        tracing::info!("Shutdown signal sent to all cores");
        Ok(())
    }

    /// Cancel a task
    pub fn cancel_task(&self, task_id: TaskId) -> Result<()> {
        // Check if runtime is running
        let state = self.state.load(Ordering::Acquire);
        if state != RuntimeState::Running as u8 {
            return Err(RuntimeError::TaskFailed(
                "Runtime is not running".to_string(),
            ));
        }

        // Send cancel message to all cores (task might be on any core)
        // In a more sophisticated implementation, we would track which core each task is on
        for sender in &self.core_senders {
            sender.push(CoreMessage::CancelTask(task_id));
        }

        tracing::info!("Task cancellation requested for task {:?}", task_id);
        Ok(())
    }

    /// Send ping messages between all CPU cores
    pub fn ping_all(&self) -> Result<()> {
        // Send ping messages between all pairs of cores
        for (from_core, sender) in self.core_senders.iter().enumerate() {
            for to_core in 0..self.num_cores {
                if from_core != to_core {
                    sender.push(CoreMessage::Ping { from_core });
                }
            }
        }

        Ok(())
    }

    /// Wait for all cores to complete
    pub fn join(mut self) -> Result<()> {
        tracing::info!("Waiting for all cores to complete");

        for (core_id, handle) in self.join_handles.drain(..).enumerate() {
            match handle.join() {
                Ok(Ok(())) => {
                    tracing::debug!("Core {} completed successfully", core_id);
                }
                Ok(Err(e)) => {
                    tracing::error!("Core {} failed: {:?}", core_id, e);
                    return Err(e);
                }
                Err(e) => {
                    tracing::error!("Failed to join core {} thread: {:?}", core_id, e);
                    return Err(RuntimeError::TaskFailed(format!(
                        "Thread join failed: {:?}",
                        e
                    )));
                }
            }
        }

        // Set state to terminated
        self.state
            .store(RuntimeState::Terminated as u8, Ordering::Release);

        tracing::info!("All cores completed successfully");
        Ok(())
    }

    /// Block on a future using the runtime
    pub fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        // For block_on, we need a different approach since we can't easily
        // integrate with our thread-per-core model. For now, use a simple
        // executor on the current thread.
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        let mut future = Box::pin(future);

        // Create a dummy waker
        fn dummy_raw_waker() -> RawWaker {
            fn dummy_clone(_: *const ()) -> RawWaker {
                dummy_raw_waker()
            }
            fn dummy_wake(_: *const ()) {}
            fn dummy_wake_by_ref(_: *const ()) {}
            fn dummy_drop(_: *const ()) {}

            const VTABLE: RawWakerVTable =
                RawWakerVTable::new(dummy_clone, dummy_wake, dummy_wake_by_ref, dummy_drop);

            RawWaker::new(std::ptr::null(), &VTABLE)
        }

        let waker = unsafe { Waker::from_raw(dummy_raw_waker()) };
        let mut context = Context::from_waker(&waker);

        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(result) => return result,
                Poll::Pending => {
                    thread::yield_now();
                }
            }
        }
    }

    /// Block on a future on a specific CPU core
    pub fn block_on_cpu<F>(&self, core_id: usize, future: F) -> Result<F::Output>
    where
        F: Future,
    {
        // Validate core ID
        if core_id >= self.num_cores {
            return Err(RuntimeError::TaskFailed(format!(
                "Invalid core ID: {}",
                core_id
            )));
        }

        // For now, we'll just use the same implementation as block_on
        // In a more sophisticated implementation, we might want to run this on the specific core
        Ok(self.block_on(future))
    }

    /// Get runtime statistics
    pub fn stats(&self) -> RuntimeStats {
        RuntimeStats::new(
            self.num_cores,
            self.state.load(Ordering::Acquire) == RuntimeState::ShuttingDown as u8
                || self.state.load(Ordering::Acquire) == RuntimeState::Terminated as u8,
        )
    }

    /// Get the number of CPU cores
    pub fn cpu_count(&self) -> usize {
        self.num_cores
    }
}

/// Runtime statistics
#[derive(Debug, Clone)]
pub struct RuntimeStats {
    pub num_cores: usize,
    pub is_shutdown: bool,
}

impl RuntimeStats {
    /// Create new runtime statistics
    fn new(num_cores: usize, is_shutdown: bool) -> Self {
        Self {
            num_cores,
            is_shutdown,
        }
    }
}

impl Drop for MultiCoreRuntime {
    fn drop(&mut self) {
        let state = self.state.load(Ordering::Acquire);
        if state == RuntimeState::Running as u8 {
            tracing::warn!("Runtime dropped without explicit shutdown");
            let _ = self.shutdown();
        }
    }
}

/// Global runtime instance for convenience functions
static GLOBAL_RUNTIME: std::sync::OnceLock<Arc<MultiCoreRuntime>> = std::sync::OnceLock::new();

/// Initialize global runtime
pub fn init_runtime(num_cores: Option<usize>) -> Result<()> {
    let runtime = MultiCoreRuntime::new(num_cores)?;
    GLOBAL_RUNTIME
        .set(runtime)
        .map_err(|_| RuntimeError::TaskFailed("Global runtime already initialized".to_string()))?;
    Ok(())
}

/// Get global runtime reference
pub fn global_runtime() -> Result<&'static Arc<MultiCoreRuntime>> {
    GLOBAL_RUNTIME.get().ok_or(RuntimeError::NotInitialized)
}

/// Spawn task on global runtime
pub fn spawn<F>(future: F) -> Result<TaskId>
where
    F: Future<Output = ()> + Send + 'static,
{
    global_runtime()?.spawn(future)
}

/// Shutdown global runtime
pub fn shutdown() -> Result<()> {
    global_runtime()?.shutdown()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_runtime_creation() {
        let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
        let stats = runtime.stats();
        assert_eq!(stats.num_cores, 2);
        assert!(!stats.is_shutdown);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_task_execution() {
        let runtime = MultiCoreRuntime::new(Some(2)).unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let _task_id = runtime
            .spawn(async move {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();

        // Give task time to execute
        thread::sleep(Duration::from_millis(100));

        runtime.shutdown().unwrap();
        match Arc::try_unwrap(runtime) {
            Ok(rt) => rt.join().unwrap(),
            Err(_) => panic!("Failed to get unique ownership of runtime"),
        }

        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_shutdown() {
        let runtime = MultiCoreRuntime::new(Some(1)).unwrap();
        runtime.shutdown().unwrap();
        match Arc::try_unwrap(runtime) {
            Ok(rt) => rt.join().unwrap(),
            Err(_) => panic!("Failed to get unique ownership of runtime"),
        }
    }
}
