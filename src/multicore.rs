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

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant};

use crate::cpu::{
    clear_current_io_state, clear_current_timer, set_current_io_state, set_current_task_id,
    set_current_timer, CpuIoState,
};
use crate::error::{Result, RuntimeError};
use crate::io::{CompletionKind, IoError, IoProvider, IoToken, Op};
use crate::task::Task;
use crate::timer::TimerWheel;
use crate::waker::TaskId;

/// Type alias for the IO backend completion type
type IoCompletion = (IoToken, Op, std::result::Result<CompletionKind, IoError>);
const CORE_MESSAGE_QUEUE_CAPACITY: usize = 4096;

/// Global task ID generator
static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

/// Generate unique task ID
fn next_task_id() -> TaskId {
    TaskId(NEXT_TASK_ID.fetch_add(1, Ordering::Release))
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
    /// Local task queue keyed by task ID. Readiness is tracked separately.
    task_queue: rustc_hash::FxHashMap<TaskId, Task>,
    /// Local ready queue populated by task wakers.
    /// Box gives stable address for Waker raw pointer.
    ready_queue: VecDeque<TaskId>,
    /// Message inbox from other cores
    message_inbox: Receiver<CoreMessage>,
    /// Local timer wheel
    timer_wheel: TimerWheel,
    /// Per-core I/O state — also holds the io_backend via `io_state.io_backend`.
    /// NOT wrapped in Arc: access is exclusively through thread-local raw pointer,
    /// so Arc provides zero benefit and adds atomic refcount overhead.
    io_state: CpuIoState,
    /// Shutdown flag
    shutdown: Arc<AtomicBool>,
    /// Task counter for this core
    local_task_count: usize,
    /// Countdown for polling cross-core messages while the shard is busy.
    message_poll_ticks: usize,
}

impl CpuCore {
    /// Create new CPU core
    fn new(
        id: usize,
        io_backend: Arc<dyn IoProvider<Completion = IoCompletion>>,
        shutdown: Arc<AtomicBool>,
        message_inbox: Receiver<CoreMessage>,
    ) -> Self {
        let mut io_state = CpuIoState::default();
        io_state.io_backend = io_backend;

        Self {
            id,
            task_queue: rustc_hash::FxHashMap::default(),
            ready_queue: VecDeque::with_capacity(256),
            message_inbox,
            timer_wheel: TimerWheel::new(1024, 1),
            io_state,
            shutdown,
            local_task_count: 0,
            message_poll_ticks: 0,
        }
    }

    /// Main event loop - shared-nothing execution
    fn run(&mut self) -> Result<()> {
        // Bind to CPU core for optimal cache locality
        self.bind_to_cpu()?;
        set_current_io_state(&self.io_state);
        set_current_timer(&mut self.timer_wheel);

        struct RuntimeContextGuard;
        impl Drop for RuntimeContextGuard {
            fn drop(&mut self) {
                clear_current_timer();
                clear_current_io_state();
            }
        }
        let _context_guard = RuntimeContextGuard;

        tracing::info!("CPU core {} started", self.id);

        /// Maximum inner-loop iterations before yielding to I/O (prevents
        /// starvation of completions and timers during a busy-task storm).
        const MAX_BUSY_ROUNDS: u32 = 64;

        while !self.shutdown.load(Ordering::Acquire) {
            // --- INNER LOOP: process all available work, submit I/O, don't wait ---
            let mut busy_rounds: u32 = 0;
            loop {
                let mut work_done = false;

                // 1. Process inter-core messages.
                //    Only check the cross-core channel when the ready queue is
                //    empty (starving for work) or every 64th tick — avoids
                //    touching the shared channel on every hot-loop iteration.
                if self.ready_queue.is_empty() || self.message_poll_ticks == 0 {
                    work_done |= self.process_messages();
                    self.message_poll_ticks = 63;
                } else {
                    self.message_poll_ticks -= 1;
                }

                // 2. Process I/O completions — may wake tasks into ready_queue.
                work_done |= self.process_io_completions();

                // 3. Execute ready tasks — may submit new SQEs.
                work_done |= self.execute_tasks();

                // 4. Submit pending SQEs to kernel (no wait).
                //    During busy periods this lets the kernel start working on
                //    I/O immediately instead of deferring it to park().
                if work_done {
                    self.io_state.io_backend.flush();
                }

                // 5. Process expired timers — they also put tasks into ready_queue.
                work_done |= self.process_timers();

                // 6. Nothing left to do — exit inner loop, go park (wait for I/O).
                if !work_done {
                    break;
                }

                // 7. Bounding: prevent I/O starvation.  If we've been busy for
                //    MAX_BUSY_ROUNDS without a break, force a park to let the
                //    kernel push completions and timers fire.
                busy_rounds += 1;
                if busy_rounds >= MAX_BUSY_ROUNDS {
                    break;
                }
            }

            // --- OUTER: park (submit_and_wait, blocks for I/O) ---
            // We've exhausted local work; block until at least one completion
            // arrives or the 1ms timeout fires.
            let n_completions = self.io_state.io_backend.park(Duration::from_millis(1));

            if n_completions > 0 {
                let waker = futures::task::noop_waker();
                let mut context = Context::from_waker(&waker);
                if let Poll::Ready(completions) =
                    self.io_state.io_backend.drain_completions(&mut context)
                {
                    for (token, _op, result) in completions {
                        unsafe { self.io_state.insert_completed(token, result) };
                        if let Some(waker) = unsafe { self.io_state.take_waker(token) } {
                            waker.wake();
                        }
                    }
                }
            }
            // Execute tasks that were just woken by the park+drain
            self.execute_tasks();
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
            match self.message_inbox.try_recv() {
                Ok(CoreMessage::Task { id, future }) => {
                    tracing::debug!(
                        "Core {} got task {:?}, {} ready",
                        self.id,
                        id,
                        self.ready_queue.len()
                    );
                    let task = Task::from_pinned(id, future);
                    self.task_queue.insert(id, task);
                    self.ready_queue.push_back(id);
                    self.local_task_count += 1;
                    processed += 1;
                }
                Ok(CoreMessage::Ping { from_core }) => {
                    tracing::trace!("Received ping from core {}", from_core);
                    processed += 1;
                }
                Ok(CoreMessage::Shutdown) => {
                    self.shutdown.store(true, Ordering::Release);
                    processed += 1;
                    break;
                }
                Ok(CoreMessage::CancelTask(task_id)) => {
                    // Remove task from queue if it exists
                    if self.task_queue.remove(&task_id).is_some() {
                        self.local_task_count = self.local_task_count.saturating_sub(1);
                    }
                    tracing::trace!("Cancelled task {:?}", task_id);
                    processed += 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        processed > 0
    }

    /// Execute local tasks — in-place poll, no remove/insert.
    fn execute_tasks(&mut self) -> bool {
        let mut executed = 0;

        while let Some(task_id) = self.ready_queue.pop_front() {
            let removed = {
                // In-place poll: borrow task mutably from task_queue.
                // Scope ensures the mutable borrow is released before remove().
                let task = match self.task_queue.get_mut(&task_id) {
                    Some(t) => t,
                    None => continue, // stale wake after cancellation
                };

                let waker = task.get_or_create_waker(&self.ready_queue as *const VecDeque<TaskId>);
                let mut context = Context::from_waker(&waker);
                set_current_task_id(Some(task_id));
                let poll_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    task.poll(&mut context)
                }));
                set_current_task_id(None);

                executed += 1;
                match poll_result {
                    Ok(Poll::Ready(())) | Err(_) => {
                        self.local_task_count = self.local_task_count.saturating_sub(1);
                        true // remove from task_queue
                    }
                    Ok(Poll::Pending) => {
                        false // stays in task_queue
                    }
                }
            };

            if removed {
                self.task_queue.remove(&task_id);
            }
        }

        executed > 0
    }

    /// Process I/O completions — tries inline poll when the waiting task is known.
    /// Completion-based: io_uring returns completed operations, this processes them.
    /// Returns `true` if any progress was made.
    fn process_io_completions(&mut self) -> bool {
        let waker = futures::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        match self.io_state.io_backend.drain_completions(&mut context) {
            Poll::Ready(completions) => {
                if completions.is_empty() {
                    return false;
                }

                let mut needs_flush = false;
                for (token, _op, result) in completions {
                    // Try inline completion: if we know which task is waiting,
                    // insert completion and poll it immediately.
                    let task_id = unsafe { self.io_state.take_pending_task(token) };
                    if let Some(tid) = task_id {
                        // In-place inline poll — no remove/insert.
                        let removed = {
                            let task = match self.task_queue.get_mut(&tid) {
                                Some(t) => t,
                                None => {
                                    // Task was already removed (completed/cleaned up)
                                    // Fall through to waker path below.
                                    // DO NOT take_waker() here; the caller's waker
                                    // in io_wakers[] is still live.
                                    continue;
                                }
                            };

                            // Remove the waker that IoFuture::poll stored
                            // (it will be re-created from our cached waker
                            // if poll returns Pending).
                            unsafe { self.io_state.take_waker(token) };
                            unsafe { self.io_state.insert_completed(token, result) };

                            let waker = task
                                .get_or_create_waker(&self.ready_queue as *const VecDeque<TaskId>);
                            let mut ctx = Context::from_waker(&waker);
                            set_current_task_id(Some(tid));
                            let poll_result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    task.poll(&mut ctx)
                                }));
                            set_current_task_id(None);

                            match poll_result {
                                Ok(Poll::Ready(())) => {
                                    self.local_task_count = self.local_task_count.saturating_sub(1);
                                    true
                                }
                                Ok(Poll::Pending) => {
                                    false // stays in task_queue
                                }
                                Err(payload) => {
                                    self.local_task_count = self.local_task_count.saturating_sub(1);
                                    tracing::error!(
                                        "Task {:?} panicked inline on core {}",
                                        tid,
                                        self.id
                                    );
                                    drop(payload);
                                    true
                                }
                            }
                        };
                        if removed {
                            self.task_queue.remove(&tid);
                        }
                        needs_flush = true;
                        continue;
                    }

                    // No task_id, or task not found: store completion in the
                    // completed_io map and wake the waiting task via its waker.
                    unsafe { self.io_state.insert_completed(token, result) };
                    if let Some(waker) = unsafe { self.io_state.take_waker(token) } {
                        waker.wake();
                    }
                }

                // Flush any SQEs that inline-polled tasks may have submitted.
                // Without this, those SQEs sit in the submission queue until
                // the next event-loop iteration's explicit flush(), adding
                // one full iteration of latency to every I/O chain.
                if needs_flush {
                    self.io_state.io_backend.flush();
                }

                true
            }
            Poll::Pending => false,
        }
    }

    /// Process timer events
    fn process_timers(&mut self) -> bool {
        if !self.timer_wheel.has_pending() {
            return false;
        }

        let mut ready = Vec::with_capacity(crate::config::EXPECTED_WAKEUP_COUNT);
        self.timer_wheel.expire(Instant::now(), &mut ready);
        if ready.is_empty() {
            return false;
        }

        for waker in ready {
            waker.wake();
        }
        true
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
    core_senders: Vec<Sender<CoreMessage>>,
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
            let core_shutdown = Arc::new(AtomicBool::new(false));
            let (sender, receiver) = bounded(CORE_MESSAGE_QUEUE_CAPACITY);
            core_senders.push(sender.clone());

            // Spawn thread for this core
            let handle = thread::Builder::new()
                .name(format!("miniss-core-{core_id}"))
                .spawn(move || {
                    // io_uring instances are thread-affine; create the backend inside
                    // the worker thread that will submit and poll its operations.
                    let io_backend = MultiCoreRuntime::create_io_backend(core_id)?;
                    let mut core =
                        CpuCore::new(core_id, io_backend, core_shutdown.clone(), receiver);
                    let result = core.run();
                    // Set shutdown flag when core exits
                    core_shutdown.store(true, Ordering::Relaxed);
                    result
                })
                .map_err(|e| {
                    RuntimeError::TaskFailed(format!("Failed to spawn core thread: {e}"))
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
    fn create_io_backend(core_id: usize) -> Result<Arc<dyn IoProvider<Completion = IoCompletion>>> {
        #[cfg(all(target_os = "linux", io_backend = "io_uring", feature = "io_uring"))]
        {
            match crate::io::uring::UringBackend::new(4096) {
                Ok(uring) => {
                    tracing::debug!("Core {} using io_uring backend", core_id);
                    return Ok(Arc::new(uring));
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
                    return Ok(Arc::new(kqueue));
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
                    return Ok(Arc::new(epoll));
                }
                Err(e) => {
                    tracing::warn!("Core {} failed to create epoll: {}", core_id, e);
                }
            }
        }

        // All IO backends failed - this is a critical error
        tracing::error!("Core {}: All IO backends failed to initialize", core_id);
        Err(RuntimeError::IoFailed(std::io::Error::other(
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
        let core_id = self.next_core.fetch_add(1, Ordering::AcqRel) % self.num_cores;

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
                "Invalid core ID: {core_id}"
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

        match self.core_senders[core_id].try_send(message) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                return Err(RuntimeError::TaskFailed(format!(
                    "Core {core_id} message queue is full"
                )));
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(RuntimeError::TaskFailed(format!(
                    "Core {core_id} worker is not running"
                )));
            }
        }

        tracing::trace!("Task {:?} submitted to core {}", task_id, core_id);
        Ok(task_id)
    }

    /// Install a signal handler that sends shutdown to all cores on SIGINT/SIGTERM.
    /// The handler runs in a dedicated thread and sends CoreMessage::Shutdown
    /// to every core's message inbox.  The second signal forces an immediate exit.
    pub fn install_signal_handler(&self) {
        use crate::signal::SignalHandler;
        let shutdown = Arc::new(AtomicBool::new(false));
        let handler = SignalHandler::with_cpu_handles(shutdown, self.core_senders.clone());
        handler.start();
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
                Ordering::Acquire, // Changed from Relaxed to Acquire for failure ordering
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
            let _ = sender.send(CoreMessage::Shutdown);
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

        tracing::info!("Task cancellation requested for task {:?}", task_id);
        for sender in &self.core_senders {
            let _ = sender.try_send(CoreMessage::CancelTask(task_id));
        }
        Ok(())
    }

    /// Send ping messages between all CPU cores
    pub fn ping_all(&self) -> Result<()> {
        // Send ping messages between all pairs of cores
        for (from_core, sender) in self.core_senders.iter().enumerate() {
            for to_core in 0..self.num_cores {
                if from_core != to_core {
                    let _ = sender.try_send(CoreMessage::Ping { from_core });
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
                        "Thread join failed: {e:?}"
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
        crate::executor::Runtime::new().block_on(future) // arch-audit: allow control-plane compatibility wrapper
    }

    /// Block on a future on a specific CPU core
    pub fn block_on_cpu<F>(&self, core_id: usize, future: F) -> Result<F::Output>
    where
        F: Future,
    {
        // Validate core ID
        if core_id >= self.num_cores {
            return Err(RuntimeError::TaskFailed(format!(
                "Invalid core ID: {core_id}"
            )));
        }

        // For now, we'll just use the same implementation as block_on
        // In a more sophisticated implementation, we might want to run this on the specific core
        Ok(self.block_on(future)) // arch-audit: allow control-plane compatibility wrapper
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
                counter_clone.fetch_add(1, Ordering::Release);
            })
            .unwrap();

        // Give task time to execute
        thread::sleep(Duration::from_millis(100));

        runtime.shutdown().unwrap();
        match Arc::try_unwrap(runtime) {
            Ok(rt) => rt.join().unwrap(),
            Err(_) => panic!("Failed to get unique ownership of runtime"),
        }

        assert_eq!(counter.load(Ordering::Acquire), 1);
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
