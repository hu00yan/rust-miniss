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

use crossbeam_queue::ArrayQueue;
use std::cell::{Cell, RefCell};
use std::future::Future;
#[cfg(target_os = "linux")]
use std::mem::size_of;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::task::Poll;
use std::thread;
use std::time::{Duration, Instant};

use crate::cpu::{current_core_id, set_current_task_id, CpuIoState, RuntimeContextGuard};
use crate::error::{Result, RuntimeError};
use crate::io::{CompletionKind, IoError, IoProvider, IoToken};
use crate::task::{DetachedTaskFuture, Task};
use crate::timer::TimerWheel;
use crate::waker::{ReadyQueue, TaskId};

/// Type alias for the IO backend completion type
type IoCompletion = (IoToken, std::result::Result<CompletionKind, IoError>);
// Local tasks never cross the accepting worker boundary.  Keep this queue
// explicitly `!Send` so the API cannot accidentally migrate shard-local
// state through the cross-core message path.
type LocalSpawn = (TaskId, Pin<Box<dyn Future<Output = ()>>>);
type LocalFuture = (TaskId, Pin<Box<dyn Future<Output = ()>>>);
const CORE_MESSAGE_QUEUE_CAPACITY: usize = 4096;
const SPAWN_BACKPRESSURE_RETRY_INTERVAL: Duration = Duration::from_millis(1);

thread_local! {
    static LOCAL_SPAWN_QUEUE: RefCell<Vec<LocalSpawn>> = const { RefCell::new(Vec::new()) };
    static LOCAL_SPAWN_COUNT: Cell<usize> = const { Cell::new(0) };
}

fn push_local_spawn(task_id: TaskId, future: Pin<Box<dyn Future<Output = ()>>>) {
    LOCAL_SPAWN_QUEUE.with(|queue| queue.borrow_mut().push((task_id, future)));
    LOCAL_SPAWN_COUNT.with(|count| count.set(count.get() + 1));
}

#[cfg(target_os = "linux")]
fn create_wakeup_eventfd() -> Result<OwnedFd> {
    // SAFETY: eventfd creates a fresh fd owned by this function; we wrap it
    // immediately in OwnedFd on success and keep it thread-local.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(RuntimeError::TaskFailed(format!(
            "Failed to create core wakeup eventfd: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: `fd` is the owned descriptor returned by eventfd above and has
    // not been wrapped elsewhere.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn notify_wakeup_eventfd(fd: RawFd) {
    let value: u64 = 1;
    let ptr = (&value as *const u64).cast::<libc::c_void>();
    let len = size_of::<u64>();
    // SAFETY: best-effort wakeup on a valid non-blocking eventfd.
    unsafe {
        let _ = libc::write(fd, ptr, len);
    }
}

#[cfg(target_os = "linux")]
fn drain_wakeup_eventfd(fd: RawFd) {
    let mut value: u64 = 0;
    let ptr = (&mut value as *mut u64).cast::<libc::c_void>();
    let len = size_of::<u64>();
    // SAFETY: best-effort drain on a valid non-blocking eventfd.
    unsafe {
        let _ = libc::read(fd, ptr, len);
    }
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
    /// Control-plane metrics snapshot request.
    CollectMetrics {
        reply: std::sync::mpsc::Sender<CoreMetricsSnapshot>,
    },
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
            Self::CollectMetrics { .. } => write!(f, "CollectMetrics"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CoreMetricsSnapshot {
    pub core_id: usize,
    pub local_task_count: usize,
    pub ready_queue_len: usize,
    pub max_ready_queue_len: usize,
    pub tasks_spawned: u64,
    pub tasks_completed: u64,
    pub tasks_polled: u64,
    pub io_completions: u64,
    pub messages_processed: u64,
    pub message_batches: u64,
    pub max_message_inbox_len: usize,
    pub busy_rounds: u64,
    pub flushes: u64,
    pub flush_total_us: u64,
    pub max_flush_us: u64,
    pub park_flushes: u64,
    pub park_flush_total_us: u64,
    pub max_park_flush_us: u64,
    pub io_completion_batches: u64,
    pub io_completion_total_ns: u64,
    pub max_io_completion_batch_ns: u64,
    pub max_io_completion_batch_len: usize,
    pub inline_poll_total_ns: u64,
    pub max_inline_poll_ns: u64,
    pub wake_total_ns: u64,
    pub max_wake_ns: u64,
    pub parks: u64,
    pub park_timeouts: u64,
    pub park_completions: u64,
    pub park_total_us: u64,
    pub max_park_us: u64,
    pub timer_expirations: u64,
    pub inline_completion_hits: u64,
    pub waker_wakeups: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeMetrics {
    pub cores: Vec<CoreMetricsSnapshot>,
}

#[derive(Debug, Default)]
struct CoreMetrics {
    tasks_spawned: u64,
    tasks_completed: u64,
    tasks_polled: u64,
    io_completions: u64,
    messages_processed: u64,
    message_batches: u64,
    max_ready_queue_len: usize,
    max_message_inbox_len: usize,
    busy_rounds: u64,
    flushes: u64,
    flush_total_us: u64,
    max_flush_us: u64,
    park_flushes: u64,
    park_flush_total_us: u64,
    max_park_flush_us: u64,
    io_completion_batches: u64,
    io_completion_total_ns: u64,
    max_io_completion_batch_ns: u64,
    max_io_completion_batch_len: usize,
    inline_poll_total_ns: u64,
    max_inline_poll_ns: u64,
    wake_total_ns: u64,
    max_wake_ns: u64,
    parks: u64,
    park_timeouts: u64,
    park_completions: u64,
    park_total_us: u64,
    max_park_us: u64,
    timer_expirations: u64,
    inline_completion_hits: u64,
    waker_wakeups: u64,
}

/// Individual CPU core executor - completely independent
pub struct CpuCore {
    /// Core ID (matches physical CPU)
    id: usize,
    /// Local task queue keyed by task sequence. Readiness is tracked separately.
    task_queue: Vec<Option<Task>>,
    /// Futures submitted by `spawn_local`; these are polled only on this
    /// worker and therefore intentionally do not require `Send`.
    local_futures: Vec<LocalFuture>,
    /// Local ready queue populated by task wakers.
    /// Bounded Arc-backed queue receives wakes from escaped wakers.
    ready_queue: ReadyQueue,
    /// Message inbox from other cores
    message_inbox: Receiver<CoreMessage>,
    /// Local timer wheel
    timer_wheel: TimerWheel,
    /// Idle park timeout, configured at runtime startup.
    park_timeout: Duration,
    /// Per-core park wakeup flag, shared with the runtime control plane.
    #[cfg(target_os = "linux")]
    sleeping: Arc<AtomicBool>,
    #[cfg(target_os = "linux")]
    /// Eventfd used to wake the worker when a cross-core message arrives.
    wakeup_eventfd: RawFd,
    #[cfg(target_os = "linux")]
    /// Wake poll armed while the worker is parked.
    wakeup_poll_armed: bool,
    #[cfg(target_os = "linux")]
    /// Token for the armed wake poll, so the completion can be ignored.
    wakeup_poll_token: Option<IoToken>,
    /// Per-core I/O state — also holds the io_backend via `io_state.io_backend`.
    /// Owned directly by worker; never shared across shards.
    io_state: CpuIoState,
    /// Shutdown flag
    shutdown: Arc<AtomicBool>,
    /// Escalation flag set by the control plane after the graceful deadline.
    /// The worker owns and drops all remaining task/I/O state before returning.
    force_shutdown: Arc<AtomicBool>,
    /// Graceful drain mode: reject new control-plane spawns at the runtime
    /// level, but let already queued and running shard-local tasks finish.
    draining: bool,
    /// Task counter for this core
    local_task_count: usize,
    /// Countdown for polling cross-core messages while the shard is busy.
    message_poll_ticks: usize,
    /// Maximum local tasks polled in one turn before returning to the event loop.
    task_poll_budget: usize,
    /// Shard-local metrics. Aggregated only by explicit control-plane request.
    metrics: CoreMetrics,
}

impl CpuCore {
    #[inline]
    fn ensure_task_capacity(&mut self, idx: usize) {
        if idx >= self.task_queue.len() {
            self.task_queue.resize_with(idx + 1, || None);
        }
    }

    /// Create new CPU core. The platform-gated constructor keeps the driver,
    /// wakeup fd, and ownership flags explicit; the argument count is kept
    /// local rather than introducing a shared configuration object.
    #[allow(clippy::too_many_arguments)]
    fn new(
        id: usize,
        io_backend: Box<dyn IoProvider<Completion = IoCompletion>>,
        shutdown: Arc<AtomicBool>,
        force_shutdown: Arc<AtomicBool>,
        #[cfg(target_os = "linux")] sleeping: Arc<AtomicBool>,
        message_inbox: Receiver<CoreMessage>,
        park_timeout: Duration,
        #[cfg(target_os = "linux")] wakeup_eventfd: RawFd,
    ) -> Self {
        let mut io_state = CpuIoState::default();
        io_state.io_backend = io_backend;

        Self {
            id,
            task_queue: Vec::with_capacity(256),
            local_futures: Vec::new(),
            ready_queue: Arc::new(ArrayQueue::new(65_536)),
            message_inbox,
            timer_wheel: TimerWheel::new(1024, 1),
            park_timeout,
            #[cfg(target_os = "linux")]
            sleeping,
            #[cfg(target_os = "linux")]
            wakeup_eventfd,
            #[cfg(target_os = "linux")]
            wakeup_poll_armed: false,
            #[cfg(target_os = "linux")]
            wakeup_poll_token: None,
            io_state,
            shutdown,
            force_shutdown,
            draining: false,
            local_task_count: 0,
            message_poll_ticks: 0,
            task_poll_budget: crate::config::task_poll_budget(),
            metrics: CoreMetrics::default(),
        }
    }

    fn metrics_snapshot(&self) -> CoreMetricsSnapshot {
        CoreMetricsSnapshot {
            core_id: self.id,
            local_task_count: self.local_task_count,
            ready_queue_len: self.ready_queue.len(),
            max_ready_queue_len: self.metrics.max_ready_queue_len,
            tasks_spawned: self.metrics.tasks_spawned,
            tasks_completed: self.metrics.tasks_completed,
            tasks_polled: self.metrics.tasks_polled,
            io_completions: self.metrics.io_completions,
            messages_processed: self.metrics.messages_processed,
            message_batches: self.metrics.message_batches,
            max_message_inbox_len: self.metrics.max_message_inbox_len,
            busy_rounds: self.metrics.busy_rounds,
            flushes: self.metrics.flushes,
            flush_total_us: self.metrics.flush_total_us,
            max_flush_us: self.metrics.max_flush_us,
            park_flushes: self.metrics.park_flushes,
            park_flush_total_us: self.metrics.park_flush_total_us,
            max_park_flush_us: self.metrics.max_park_flush_us,
            io_completion_batches: self.metrics.io_completion_batches,
            io_completion_total_ns: self.metrics.io_completion_total_ns,
            max_io_completion_batch_ns: self.metrics.max_io_completion_batch_ns,
            max_io_completion_batch_len: self.metrics.max_io_completion_batch_len,
            inline_poll_total_ns: self.metrics.inline_poll_total_ns,
            max_inline_poll_ns: self.metrics.max_inline_poll_ns,
            wake_total_ns: self.metrics.wake_total_ns,
            max_wake_ns: self.metrics.max_wake_ns,
            parks: self.metrics.parks,
            park_timeouts: self.metrics.park_timeouts,
            park_completions: self.metrics.park_completions,
            park_total_us: self.metrics.park_total_us,
            max_park_us: self.metrics.max_park_us,
            timer_expirations: self.metrics.timer_expirations,
            inline_completion_hits: self.metrics.inline_completion_hits,
            waker_wakeups: self.metrics.waker_wakeups,
        }
    }

    #[cfg(target_os = "linux")]
    fn arm_wakeup_poll(&mut self) {
        if self.wakeup_poll_armed {
            return;
        }

        let token = self.io_state.io_backend.submit(crate::io::Op::PollAdd {
            fd: self.wakeup_eventfd,
            events: libc::POLLIN as u32,
            multishot: false,
        });
        self.wakeup_poll_token = Some(token);
        self.wakeup_poll_armed = true;
        self.io_state.io_backend.flush();
    }

    #[cfg(target_os = "linux")]
    fn clear_wakeup_poll(&mut self) {
        self.wakeup_poll_armed = false;
        self.wakeup_poll_token = None;
        drain_wakeup_eventfd(self.wakeup_eventfd);
    }

    /// Main event loop - shared-nothing execution
    fn run(&mut self) -> Result<()> {
        // Bind to CPU core for optimal cache locality
        self.bind_to_cpu()?;
        // SAFETY: CpuCore owns both fields for the whole run invocation; the
        // context guard drops before CpuCore can be moved or destroyed.
        let _runtime_context = unsafe {
            RuntimeContextGuard::enter(&self.io_state, &mut self.timer_wheel, Some(self.id))
        };
        struct LocalSpawnContextGuard;
        impl Drop for LocalSpawnContextGuard {
            fn drop(&mut self) {
                LOCAL_SPAWN_QUEUE.with(|queue| queue.borrow_mut().clear());
                LOCAL_SPAWN_COUNT.with(|count| count.set(0));
            }
        }
        let _local_spawn_context = LocalSpawnContextGuard;

        tracing::info!("CPU core {} started", self.id);

        /// Maximum inner-loop iterations before yielding to I/O (prevents
        /// starvation of completions and timers during a busy-task storm).
        const MAX_BUSY_ROUNDS: u32 = 96;
        const MESSAGE_POLL_TICKS_LIGHT: usize = 3;
        const MESSAGE_POLL_TICKS_BUSY: usize = 15;
        let mut ready_timers = Vec::with_capacity(crate::config::EXPECTED_WAKEUP_COUNT);

        loop {
            if self.force_shutdown.load(Ordering::Acquire) {
                // Escalated shutdown is intentionally owner-local: no control
                // thread touches task futures, timers, or driver state.
                self.task_queue.clear();
                self.local_futures.clear();
                while self.ready_queue.pop().is_some() {}
                self.local_task_count = 0;
                self.draining = true;
                break;
            }
            if self.shutdown.load(Ordering::Acquire) {
                self.draining = true;
            }

            // --- INNER LOOP: process all available work, submit I/O, don't wait ---
            let mut busy_rounds: u32 = 0;
            loop {
                let mut work_done = false;
                // 1. Process inter-core messages.
                //    Check immediately when messages are waiting, when the
                //    ready queue is empty, or every Nth tick under load.
                if self.draining || self.ready_queue.is_empty() || self.message_poll_ticks == 0 {
                    work_done |= self.process_messages();
                    self.message_poll_ticks = if self.ready_queue.len() < 8 {
                        MESSAGE_POLL_TICKS_LIGHT
                    } else {
                        MESSAGE_POLL_TICKS_BUSY
                    };
                } else {
                    self.message_poll_ticks -= 1;
                }

                // 2. Process I/O completions — may wake tasks into ready_queue.
                work_done |= self.process_io_completions(false);

                // 3. Execute ready tasks — may submit new SQEs.
                work_done |= self.execute_tasks();

                // 4. Submit pending SQEs to kernel (no wait).
                //    During busy periods this lets the kernel start working on
                //    I/O immediately instead of deferring it to park().
                if work_done {
                    self.metrics.flushes += 1;
                    let flush_started = Instant::now();
                    self.io_state.io_backend.flush();
                    let flush_elapsed_us = flush_started.elapsed().as_micros() as u64;
                    self.metrics.flush_total_us += flush_elapsed_us;
                    self.metrics.max_flush_us = self.metrics.max_flush_us.max(flush_elapsed_us);
                }

                // 5. Process expired timers — they also put tasks into ready_queue.
                if self.timer_wheel.has_pending() {
                    let now = Instant::now();
                    work_done |= self.process_timers(now, &mut ready_timers);
                }

                if !work_done && self.io_state.io_backend.pending_submission_count() > 0 {
                    self.metrics.flushes += 1;
                    let flush_started = Instant::now();
                    self.io_state.io_backend.flush();
                    let flush_elapsed_us = flush_started.elapsed().as_micros() as u64;
                    self.metrics.flush_total_us += flush_elapsed_us;
                    self.metrics.max_flush_us = self.metrics.max_flush_us.max(flush_elapsed_us);
                    work_done = true;
                }

                // 6. Nothing left to do — exit inner loop, go park (wait for I/O).
                if !work_done {
                    break;
                }

                // 7. Bounding: prevent I/O starvation.  If we've been busy for
                //    MAX_BUSY_ROUNDS without a break, force a park to let the
                //    kernel push completions and timers fire.
                busy_rounds += 1;
                self.metrics.busy_rounds += 1;
                if busy_rounds >= MAX_BUSY_ROUNDS {
                    break;
                }
            }

            if self.draining
                && self.local_task_count == 0
                && self.ready_queue.is_empty()
                && self.message_inbox.is_empty()
            {
                break;
            }

            // --- OUTER: park (submit_and_wait, blocks for I/O) ---
            // We've exhausted local work; block until at least one completion
            // arrives, a wake message lands, or the timeout fires.
            #[cfg(target_os = "linux")]
            self.sleeping.store(true, Ordering::Release);
            #[cfg(target_os = "linux")]
            if self.process_messages() {
                self.sleeping.store(false, Ordering::Release);
                continue;
            }
            #[cfg(target_os = "linux")]
            self.arm_wakeup_poll();
            self.metrics.parks += 1;
            let park_started = Instant::now();
            let n_completions = self.io_state.io_backend.park(self.park_timeout);
            let park_elapsed_us = park_started.elapsed().as_micros() as u64;
            let (park_flushes, park_flush_total_us, max_park_flush_us) =
                self.io_state.io_backend.take_park_flush_stats();
            #[cfg(target_os = "linux")]
            self.sleeping.store(false, Ordering::Release);
            self.metrics.park_total_us += park_elapsed_us;
            self.metrics.max_park_us = self.metrics.max_park_us.max(park_elapsed_us);
            self.metrics.park_flushes += park_flushes;
            self.metrics.park_flush_total_us += park_flush_total_us;
            self.metrics.max_park_flush_us = self.metrics.max_park_flush_us.max(max_park_flush_us);
            if n_completions == 0 {
                self.metrics.park_timeouts += 1;
            } else {
                self.metrics.park_completions += n_completions as u64;
            }

            if n_completions > 0 {
                let _ = self.process_io_completions(true);
            }
            // Execute tasks that were just woken by the park+drain
            if self.execute_tasks() {
                self.metrics.flushes += 1;
                let flush_started = Instant::now();
                self.io_state.io_backend.flush();
                let flush_elapsed_us = flush_started.elapsed().as_micros() as u64;
                self.metrics.flush_total_us += flush_elapsed_us;
                self.metrics.max_flush_us = self.metrics.max_flush_us.max(flush_elapsed_us);
            }

            if self.draining
                && self.local_task_count == 0
                && self.ready_queue.is_empty()
                && self.message_inbox.is_empty()
            {
                break;
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

        // SAFETY: The cpu_set_t is zero-initialized before CPU_SET, and
        // sched_setaffinity receives a pointer valid for the call duration.
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
        self.metrics.max_message_inbox_len = self
            .metrics
            .max_message_inbox_len
            .max(self.message_inbox.len());
        let ready_len = self.ready_queue.len();
        let max_messages = if self.draining || ready_len == 0 {
            256
        } else if ready_len < 8 {
            32
        } else {
            16
        };

        // Process more messages when the shard is idle; back off when it is busy.
        while processed < max_messages {
            match self.message_inbox.try_recv() {
                Ok(CoreMessage::Task { id, future }) => {
                    tracing::debug!(
                        "Core {} got task {:?}, {} ready",
                        self.id,
                        id,
                        self.ready_queue.len()
                    );
                    let mut task = Task::from_pinned(id, future);
                    let idx = id.key().slot as usize;
                    self.ensure_task_capacity(idx);
                    if self.task_queue[idx]
                        .as_ref()
                        .is_some_and(|existing| existing.id() != id)
                    {
                        tracing::warn!("Core {} rejected task {:?}: slot occupied", self.id, id);
                        processed += 1;
                        continue;
                    }
                    task.ensure_waker(self.ready_queue.clone());
                    task.schedule();
                    self.task_queue[idx] = Some(task);
                    self.metrics.max_ready_queue_len =
                        self.metrics.max_ready_queue_len.max(self.ready_queue.len());
                    self.local_task_count += 1;
                    self.metrics.tasks_spawned += 1;
                    processed += 1;
                }
                Ok(CoreMessage::Ping { from_core }) => {
                    tracing::trace!("Received ping from core {}", from_core);
                    processed += 1;
                }
                Ok(CoreMessage::Shutdown) => {
                    self.shutdown.store(true, Ordering::Release);
                    self.draining = true;
                    processed += 1;
                }
                Ok(CoreMessage::CancelTask(task_id)) => {
                    // Remove task from queue if it exists
                    let idx = task_id.key().slot as usize;
                    if let Some(slot) = self.task_queue.get_mut(idx) {
                        if slot.as_ref().is_some_and(|task| task.id() == task_id)
                            && slot.take().is_some()
                        {
                            self.local_task_count = self.local_task_count.saturating_sub(1);
                            self.metrics.tasks_completed += 1;
                        }
                    }
                    tracing::trace!("Cancelled task {:?}", task_id);
                    processed += 1;
                }
                Ok(CoreMessage::CollectMetrics { reply }) => {
                    let _ = reply.send(self.metrics_snapshot());
                    processed += 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        self.metrics.messages_processed += processed as u64;
        if processed > 0 {
            self.metrics.message_batches += 1;
        }
        processed > 0
    }

    /// Execute local tasks — in-place poll, no remove/insert.
    fn execute_tasks(&mut self) -> bool {
        let mut executed = 0;
        let mut polled = 0;

        // Local futures are shard-owned and may be !Send. Poll them on every
        // turn with a no-op waker; their next poll is bounded by the same
        // per-turn budget as regular tasks.
        let local_budget = self.task_poll_budget;
        let mut local_index = 0;
        while local_index < self.local_futures.len() && polled < local_budget {
            polled += 1;
            let poll_result = {
                let (_, future) = &mut self.local_futures[local_index];
                let waker = futures::task::noop_waker();
                let mut cx = std::task::Context::from_waker(&waker);
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    future.as_mut().poll(&mut cx)
                }))
            };
            match poll_result {
                Ok(Poll::Ready(())) | Err(_) => {
                    let _ = self.local_futures.swap_remove(local_index);
                    self.local_task_count = self.local_task_count.saturating_sub(1);
                    self.metrics.tasks_completed += 1;
                }
                Ok(Poll::Pending) => {
                    local_index += 1;
                }
            }
            executed += 1;
        }

        while polled < self.task_poll_budget {
            let Some(task_id) = self.ready_queue.pop() else {
                break;
            };
            // Count every scheduled record, including stale wakes, against
            // the turn budget. Otherwise a wake storm for cancelled tasks
            // could drain the queue without returning to I/O/messages/timers.
            polled += 1;
            let removed = {
                // In-place poll: borrow task mutably from task_queue.
                // Scope ensures the mutable borrow is released before remove().
                let idx = task_id.key().slot as usize;
                let task = match self.task_queue.get_mut(idx).and_then(Option::as_mut) {
                    Some(t) => t,
                    None => continue, // stale wake after cancellation
                };
                if task.id() != task_id {
                    continue; // stale wake after slot reuse
                }

                set_current_task_id(Some(task_id));
                let poll_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    task.poll_with_cached_waker()
                }));
                set_current_task_id(None);
                executed += 1;
                self.metrics.tasks_polled += 1;
                match poll_result {
                    Ok(Poll::Ready(())) => {
                        self.local_task_count = self.local_task_count.saturating_sub(1);
                        self.metrics.tasks_completed += 1;
                        true // remove from task_queue
                    }
                    Ok(Poll::Pending) => {
                        false // stays in task_queue
                    }
                    Err(payload) => {
                        self.local_task_count = self.local_task_count.saturating_sub(1);
                        self.metrics.tasks_completed += 1;
                        tracing::error!("Task {:?} panicked on core {}", task_id, self.id);
                        drop(payload);
                        true
                    }
                }
            };

            if removed {
                let idx = task_id.key().slot as usize;
                if let Some(slot) = self.task_queue.get_mut(idx) {
                    *slot = None;
                }
            }
        }

        let spawned_local = self.drain_local_spawns();
        executed > 0 || spawned_local
    }

    fn drain_local_spawns(&mut self) -> bool {
        if LOCAL_SPAWN_COUNT.with(Cell::get) == 0 {
            return false;
        }

        LOCAL_SPAWN_QUEUE.with(|queue| {
            let mut queue = queue.borrow_mut();
            if queue.is_empty() {
                LOCAL_SPAWN_COUNT.with(|count| count.set(0));
                return false;
            }

            for (id, future) in queue.drain(..) {
                self.local_futures.push((id, future));
                self.local_task_count += 1;
                self.metrics.tasks_spawned += 1;
            }
            LOCAL_SPAWN_COUNT.with(|count| count.set(0));

            true
        })
    }

    /// Process I/O completions — tries inline poll when the waiting task is known.
    /// Completion-based: io_uring returns completed operations, this processes them.
    /// Returns `true` if any progress was made.
    fn process_io_completions(&mut self, after_park: bool) -> bool {
        // Fused path: CQE → slot + wake, no intermediate Vec.
        // SAFETY: UringBackend is thread-affine and we are on the owning thread.
        #[cfg(target_os = "linux")]
        let wakeup_token = self.wakeup_poll_token;
        #[cfg(not(target_os = "linux"))]
        let wakeup_token: Option<IoToken> = None;

        // Empty or nearly-empty ready queues can afford a larger CQE drain batch.
        let drain_limit = if after_park {
            256
        } else if self.ready_queue.len() < 8 {
            128
        } else {
            61
        };
        let drain_started = Instant::now();
        #[cfg(target_os = "linux")]
        let (count, saw_wakeup_poll, wakeups) = unsafe {
            self.io_state
                .io_backend
                .drain_completions_fused(wakeup_token, drain_limit)
        };
        #[cfg(not(target_os = "linux"))]
        let (count, saw_wakeup_poll, wakeups) = unsafe {
            self.io_state
                .io_backend
                .drain_completions_fused(wakeup_token, drain_limit)
        };

        #[cfg(target_os = "linux")]
        if self.wakeup_poll_armed && saw_wakeup_poll {
            self.clear_wakeup_poll();
        }

        let drain_elapsed_ns = drain_started.elapsed().as_nanos() as u64;
        self.metrics.io_completion_total_ns += drain_elapsed_ns;
        self.metrics.max_io_completion_batch_ns = self
            .metrics
            .max_io_completion_batch_ns
            .max(drain_elapsed_ns);
        self.metrics.max_io_completion_batch_len =
            self.metrics.max_io_completion_batch_len.max(count);

        if count > 0 || saw_wakeup_poll {
            self.metrics.io_completion_batches += 1;
            self.metrics.io_completions += count as u64;
            self.metrics.waker_wakeups += wakeups as u64;
            true
        } else {
            false
        }
    }

    /// Process timer events
    fn process_timers(&mut self, now: Instant, ready: &mut Vec<std::task::Waker>) -> bool {
        if !self.timer_wheel.has_pending() {
            return false;
        }

        ready.clear();
        self.timer_wheel.expire(now, ready);
        if ready.is_empty() {
            return false;
        }

        self.metrics.timer_expirations += ready.len() as u64;

        for waker in ready.drain(..) {
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
    /// Per-core shutdown flags, written directly by the control plane so
    /// shutdown is not delayed behind ordinary data-plane messages.
    core_shutdowns: Vec<Arc<AtomicBool>>,
    /// Worker-owned completion acknowledgement; unlike `core_shutdowns`, this
    /// is written only after the event loop has drained or escalated.
    core_exited: Vec<Arc<AtomicBool>>,
    /// Per-core escalation flags; only workers mutate local task/driver state.
    force_shutdowns: Vec<Arc<AtomicBool>>,
    /// Per-core sleep flags; senders only write to the wake eventfd when the
    /// target core is actually parked.
    #[cfg(target_os = "linux")]
    core_sleeping: Vec<Arc<AtomicBool>>,
    #[cfg(target_os = "linux")]
    /// Per-core eventfds used to wake parked workers when messages arrive.
    core_wakeup_fds: Vec<OwnedFd>,
    /// Thread join handles
    join_handles: Vec<thread::JoinHandle<Result<()>>>,
    /// Per-core task sequence counters.
    task_counters: Vec<AtomicU64>,
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
        let mut core_shutdowns = Vec::with_capacity(num_cores);
        let mut core_exited = Vec::with_capacity(num_cores);
        let mut force_shutdowns = Vec::with_capacity(num_cores);
        #[cfg(target_os = "linux")]
        let mut core_sleeping = Vec::with_capacity(num_cores);
        #[cfg(target_os = "linux")]
        let mut core_wakeup_fds = Vec::with_capacity(num_cores);
        let mut join_handles = Vec::with_capacity(num_cores);
        let mut ready_receivers = Vec::with_capacity(num_cores);
        let mut task_counters = Vec::with_capacity(num_cores);

        // Create cores and start threads
        for core_id in 0..num_cores {
            let core_shutdown = Arc::new(AtomicBool::new(false));
            let core_exited_flag = Arc::new(AtomicBool::new(false));
            let force_shutdown = Arc::new(AtomicBool::new(false));
            #[cfg(target_os = "linux")]
            let core_sleeping_flag = Arc::new(AtomicBool::new(false));
            let (sender, receiver) = bounded(CORE_MESSAGE_QUEUE_CAPACITY);
            let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
            core_senders.push(sender.clone());
            core_shutdowns.push(core_shutdown.clone());
            core_exited.push(core_exited_flag.clone());
            force_shutdowns.push(force_shutdown.clone());
            #[cfg(target_os = "linux")]
            core_sleeping.push(core_sleeping_flag.clone());
            ready_receivers.push((core_id, ready_receiver));
            task_counters.push(AtomicU64::new(1));
            #[cfg(target_os = "linux")]
            let wakeup_fd = create_wakeup_eventfd()?;
            #[cfg(target_os = "linux")]
            let wakeup_fd_raw = wakeup_fd.as_raw_fd();
            #[cfg(target_os = "linux")]
            core_wakeup_fds.push(wakeup_fd);

            // Spawn thread for this core
            let handle = thread::Builder::new()
                .name(format!("miniss-core-{core_id}"))
                .spawn(move || {
                    // io_uring instances are thread-affine; create the backend inside
                    // the worker thread that will submit and poll its operations.
                    let io_backend = match MultiCoreRuntime::create_io_backend(core_id) {
                        Ok(io_backend) => io_backend,
                        Err(err) => {
                            let _ = ready_sender.send(Err(format!("{err:?}")));
                            return Err(err);
                        }
                    };
                    #[cfg(target_os = "linux")]
                    let mut core = {
                        let sleeping = core_sleeping_flag.clone();
                        CpuCore::new(
                            core_id,
                            io_backend,
                            core_shutdown.clone(),
                            force_shutdown.clone(),
                            sleeping,
                            receiver,
                            crate::config::park_timeout(),
                            wakeup_fd_raw,
                        )
                    };
                    #[cfg(not(target_os = "linux"))]
                    let mut core = CpuCore::new(
                        core_id,
                        io_backend,
                        core_shutdown.clone(),
                        force_shutdown.clone(),
                        receiver,
                        crate::config::park_timeout(),
                    );
                    let _ = ready_sender.send(Ok(()));
                    let result = core.run();
                    // Set shutdown flag when core exits
                    // Publish worker teardown before the coordinator observes
                    // the shutdown flag and returns from its deadline wait.
                    core_shutdown.store(true, Ordering::Release);
                    core_exited_flag.store(true, Ordering::Release);
                    result
                })
                .map_err(|e| {
                    RuntimeError::TaskFailed(format!("Failed to spawn core thread: {e}"))
                })?;

            join_handles.push(handle);
        }

        for (core_id, ready_receiver) in ready_receivers {
            match ready_receiver.recv_timeout(Duration::from_secs(2)) {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    for sender in &core_senders {
                        let _ = sender.send(CoreMessage::Shutdown);
                    }
                    for handle in join_handles {
                        let _ = handle.join();
                    }
                    return Err(RuntimeError::TaskFailed(format!(
                        "Core {core_id} worker failed to start: {err}"
                    )));
                }
                Err(err) => {
                    for sender in &core_senders {
                        let _ = sender.send(CoreMessage::Shutdown);
                    }
                    for handle in join_handles {
                        let _ = handle.join();
                    }
                    return Err(RuntimeError::TaskFailed(format!(
                        "Core {core_id} worker did not report startup: {err}"
                    )));
                }
            }
        }

        let runtime = Arc::new(Self {
            num_cores,
            core_senders,
            core_shutdowns,
            core_exited,
            force_shutdowns,
            #[cfg(target_os = "linux")]
            core_sleeping,
            #[cfg(target_os = "linux")]
            core_wakeup_fds,
            join_handles,
            task_counters,
            state: AtomicU8::new(RuntimeState::Initializing as u8),
            next_core: AtomicUsize::new(0),
        });

        // Set state to running after successful initialization
        runtime
            .state
            .store(RuntimeState::Running as u8, Ordering::Release);

        Ok(runtime)
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn notify_core(&self, core_id: usize) {
        if self.core_sleeping[core_id].load(Ordering::Acquire) {
            notify_wakeup_eventfd(self.core_wakeup_fds[core_id].as_raw_fd());
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[inline]
    fn notify_core(&self, _core_id: usize) {}

    /// Create IO backend for a specific core
    fn create_io_backend(core_id: usize) -> Result<Box<dyn IoProvider<Completion = IoCompletion>>> {
        #[cfg(all(target_os = "linux", io_backend = "io_uring"))]
        {
            match crate::io::uring::UringBackend::new(4096) {
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
        Err(RuntimeError::IoFailed(std::io::Error::other(
            "No suitable IO backend available",
        )))
    }

    /// Spawn task on optimal core
    ///
    /// This is a cross-core-capable API: the future is sent to a worker chosen
    /// by round-robin. Use `spawn_local` from within a worker task when the new
    /// task should stay on the same shard as its parent.
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

        let start_core = self.select_spawn_core();
        self.spawn_on_any_core(start_core, Box::pin(future))
    }

    /// Spawn task on a specific core
    ///
    /// This preserves explicit affinity but may still cross a core boundary if
    /// called from a different worker or from a control thread.
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

        let task_id = self.next_task_id_for_core(core_id);
        self.spawn_on_core(core_id, task_id, future)
    }

    /// Spawn task on optimal core, waiting for bounded queue capacity.
    ///
    /// Unlike `spawn`, this method does not fail immediately when the selected
    /// shard inbox is full. It retries with timer-based backpressure until the
    /// message is accepted or `timeout` expires.
    pub async fn spawn_wait<F>(&self, future: F, timeout: Duration) -> Result<TaskId>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let state = self.state.load(Ordering::Acquire);
        if state != RuntimeState::Running as u8 {
            return Err(RuntimeError::TaskFailed(
                "Runtime is not running".to_string(),
            ));
        }

        let start_core = self.select_spawn_core();
        self.send_task_with_backpressure_any(start_core, Box::pin(future), timeout)
            .await
    }

    /// Spawn task on a specific core, waiting for bounded queue capacity.
    ///
    /// This is the backpressure-aware variant of `spawn_on`; it preserves the
    /// caller's explicit shard choice and never falls back to an unbounded queue.
    pub async fn spawn_on_wait<F>(
        &self,
        core_id: usize,
        future: F,
        timeout: Duration,
    ) -> Result<TaskId>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let state = self.state.load(Ordering::Acquire);
        if state != RuntimeState::Running as u8 {
            return Err(RuntimeError::TaskFailed(
                "Runtime is not running".to_string(),
            ));
        }

        if core_id >= self.num_cores {
            return Err(RuntimeError::TaskFailed(format!(
                "Invalid core ID: {core_id}"
            )));
        }

        let task_id = self.next_task_id_for_core(core_id);
        self.send_task_with_backpressure_on_core(core_id, task_id, Box::pin(future), timeout)
            .await
    }

    /// Spawn a task on the worker core currently executing this code.
    ///
    /// This is the preferred API for connection handlers and child tasks that
    /// should keep shard affinity. It requires being called from a miniss
    /// worker thread; outside the runtime use `spawn` or `spawn_on`.
    pub fn spawn_local<F>(&self, future: F) -> Result<TaskId>
    where
        F: Future<Output = ()> + 'static,
    {
        let state = self.state.load(Ordering::Acquire);
        if state != RuntimeState::Running as u8 {
            return Err(RuntimeError::TaskFailed(
                "Runtime is not running".to_string(),
            ));
        }

        let core_id = current_core_id().ok_or_else(|| {
            RuntimeError::TaskFailed("spawn_local called outside a worker core".to_string())
        })?;
        let task_id = self.next_task_id_for_core(core_id);
        push_local_spawn(task_id, Box::pin(DetachedTaskFuture::new(future)));
        Ok(task_id)
    }

    #[inline]
    fn next_task_id_for_core(&self, core_id: usize) -> TaskId {
        let sequence = self.task_counters[core_id].fetch_add(1, Ordering::Relaxed) as u32;
        TaskId::new(core_id, sequence)
    }

    /// Pick the least-loaded core as the starting point for cross-core spawn.
    ///
    /// This is a control-plane decision on the submission side. It only reads
    /// shard inbox depth and uses round-robin as a tie-breaker to avoid bias.
    #[inline]
    fn select_spawn_core(&self) -> usize {
        let start = self.next_core.fetch_add(1, Ordering::Relaxed) % self.num_cores;
        let mut best_core = start;
        let mut best_depth = self.core_senders[start].len();

        for offset in 1..self.num_cores {
            let core_id = (start + offset) % self.num_cores;
            let depth = self.core_senders[core_id].len();
            if depth < best_depth {
                best_core = core_id;
                best_depth = depth;
                if depth == 0 {
                    break;
                }
            }
        }

        best_core
    }

    /// Internal helper to spawn a task on a specific core
    fn spawn_on_core<F>(&self, core_id: usize, task_id: TaskId, future: F) -> Result<TaskId>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        // Send task to selected core
        let message = CoreMessage::Task {
            id: task_id,
            future: Box::pin(DetachedTaskFuture::new(future)),
        };

        match self.core_senders[core_id].try_send(message) {
            Ok(()) => self.notify_core(core_id),
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

    fn spawn_on_any_core(
        &self,
        start_core: usize,
        mut future: Pin<Box<dyn Future<Output = ()> + Send>>,
    ) -> Result<TaskId> {
        let mut disconnected = 0usize;

        for offset in 0..self.num_cores {
            let core_id = (start_core + offset) % self.num_cores;
            let task_id = self.next_task_id_for_core(core_id);
            let message = CoreMessage::Task {
                id: task_id,
                future: Box::pin(DetachedTaskFuture::new(future)),
            };
            match self.core_senders[core_id].try_send(message) {
                Ok(()) => {
                    self.notify_core(core_id);
                    return Ok(task_id);
                }
                Err(TrySendError::Full(CoreMessage::Task {
                    future: returned_future,
                    ..
                })) => {
                    future = returned_future;
                }
                Err(TrySendError::Disconnected(CoreMessage::Task {
                    future: returned_future,
                    ..
                })) => {
                    future = returned_future;
                    disconnected += 1;
                }
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                    unreachable!("spawn_on_any_core only sends task messages")
                }
            }
        }

        if disconnected == self.num_cores {
            Err(RuntimeError::TaskFailed(
                "All worker cores are disconnected".to_string(),
            ))
        } else {
            Err(RuntimeError::TaskFailed(
                "All worker cores are busy".to_string(),
            ))
        }
    }

    async fn send_task_with_backpressure_any(
        &self,
        start_core: usize,
        mut future: Pin<Box<dyn Future<Output = ()> + Send>>,
        timeout: Duration,
    ) -> Result<TaskId> {
        let deadline = Instant::now() + timeout;

        loop {
            for offset in 0..self.num_cores {
                let core_id = (start_core + offset) % self.num_cores;
                let task_id = self.next_task_id_for_core(core_id);
                let message = CoreMessage::Task {
                    id: task_id,
                    future: Box::pin(DetachedTaskFuture::new(future)),
                };
                match self.core_senders[core_id].try_send(message) {
                    Ok(()) => {
                        self.notify_core(core_id);
                        tracing::trace!("Task {:?} submitted to core {}", task_id, core_id);
                        return Ok(task_id);
                    }
                    Err(TrySendError::Full(CoreMessage::Task {
                        future: returned_future,
                        ..
                    }))
                    | Err(TrySendError::Disconnected(CoreMessage::Task {
                        future: returned_future,
                        ..
                    })) => {
                        future = returned_future;
                    }
                    Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                        unreachable!("send_task_with_backpressure_any only sends task messages")
                    }
                }
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(RuntimeError::TaskFailed(
                    "Timed out waiting for worker core message queue capacity".to_string(),
                ));
            }

            let remaining = deadline.saturating_duration_since(now);
            crate::timer::sleep(remaining.min(SPAWN_BACKPRESSURE_RETRY_INTERVAL)).await;
        }
    }

    async fn send_task_with_backpressure_on_core(
        &self,
        core_id: usize,
        task_id: TaskId,
        mut future: Pin<Box<dyn Future<Output = ()> + Send>>,
        timeout: Duration,
    ) -> Result<TaskId> {
        let deadline = Instant::now() + timeout;

        loop {
            let message = CoreMessage::Task {
                id: task_id,
                future: Box::pin(DetachedTaskFuture::new(future)),
            };
            match self.core_senders[core_id].try_send(message) {
                Ok(()) => {
                    self.notify_core(core_id);
                    tracing::trace!("Task {:?} submitted to core {}", task_id, core_id);
                    return Ok(task_id);
                }
                Err(TrySendError::Full(CoreMessage::Task {
                    future: returned_future,
                    ..
                }))
                | Err(TrySendError::Disconnected(CoreMessage::Task {
                    future: returned_future,
                    ..
                })) => {
                    future = returned_future;
                }
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                    unreachable!("send_task_with_backpressure_on_core only sends task messages")
                }
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(RuntimeError::TaskFailed(format!(
                    "Timed out waiting for core {core_id} message queue capacity"
                )));
            }

            let remaining = deadline.saturating_duration_since(now);
            crate::timer::sleep(remaining.min(SPAWN_BACKPRESSURE_RETRY_INTERVAL)).await;
        }
    }

    /// Install a signal handler that sends shutdown to all cores on SIGINT/SIGTERM.
    /// The handler runs in a dedicated thread and sends CoreMessage::Shutdown
    /// to every core's message inbox.  The second signal forces an immediate exit.
    pub fn install_signal_handler(&self) {
        use crate::signal::SignalHandler;
        let shutdown = Arc::new(AtomicBool::new(false));
        let handler = SignalHandler::with_cpu_handles(
            shutdown,
            self.core_senders.clone(),
            self.core_shutdowns.clone(),
        );
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

        // Set per-core shutdown flags directly so shutdown is not starved by
        // ordinary queued task messages.
        for shutdown in &self.core_shutdowns {
            shutdown.store(true, Ordering::Release);
        }

        // Best-effort wakeup. Do not block if a bounded message queue is full:
        // the direct shutdown flag above is authoritative.
        for (core_id, sender) in self.core_senders.iter().enumerate() {
            let _ = sender.try_send(CoreMessage::Shutdown);
            self.notify_core(core_id);
        }

        tracing::info!("Shutdown signal sent to all cores");
        Ok(())
    }

    /// Initiate graceful shutdown and escalate after `timeout`.
    ///
    /// The graceful phase stops admission and lets workers drain queued work.
    /// If a permanently pending task or I/O prevents that, the control plane
    /// sets an owner-local force flag and wakes every worker.  Workers then
    /// drop their remaining futures and driver on their own thread; callers
    /// must still call [`MultiCoreRuntime::join`] to collect worker results.
    pub fn shutdown_with_timeout(&self, timeout: Duration) -> Result<()> {
        self.shutdown()?;
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .core_exited
                .iter()
                .all(|flag| flag.load(Ordering::Acquire))
            {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                tracing::warn!("graceful shutdown deadline exceeded; escalating");
                for force in &self.force_shutdowns {
                    force.store(true, Ordering::Release);
                }
                for core_id in 0..self.num_cores {
                    self.notify_core(core_id);
                }
                return Ok(());
            }
            thread::sleep((deadline - now).min(Duration::from_millis(1)));
        }
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
        // TaskKey carries authoritative owner shard.  Route one bounded
        // message there; broadcasting by sequence let neighboring shards
        // accidentally cancel unrelated tasks and hid queue backpressure.
        let core_id = task_id.cpu_id();
        let sender = self.core_senders.get(core_id).ok_or_else(|| {
            RuntimeError::TaskFailed(format!("Task owner shard {core_id} is unavailable"))
        })?;
        sender
            .try_send(CoreMessage::CancelTask(task_id))
            .map_err(|error| {
                let reason = match error {
                    TrySendError::Full(_) => "cancel queue backpressured",
                    TrySendError::Disconnected(_) => "task owner shard stopped",
                };
                RuntimeError::TaskFailed(reason.to_string())
            })?;
        self.notify_core(core_id);
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
            self.notify_core(from_core);
        }

        Ok(())
    }

    /// Collect per-core runtime metrics through control-plane shard messages.
    ///
    /// Metrics counters are shard-local; this method aggregates snapshots on
    /// demand and is not intended for per-request hot-path use.
    pub fn metrics_snapshot(&self, timeout: Duration) -> Result<RuntimeMetrics> {
        let state = self.state.load(Ordering::Acquire);
        if state != RuntimeState::Running as u8 && state != RuntimeState::ShuttingDown as u8 {
            return Err(RuntimeError::TaskFailed(
                "Runtime is not running".to_string(),
            ));
        }

        let (reply, receiver) = std::sync::mpsc::channel();
        for (core_id, sender) in self.core_senders.iter().enumerate() {
            sender
                .try_send(CoreMessage::CollectMetrics {
                    reply: reply.clone(),
                })
                .map_err(|err| {
                    RuntimeError::TaskFailed(format!(
                        "Core {core_id} metrics request failed: {err}"
                    ))
                })?;
            self.notify_core(core_id);
        }
        drop(reply);

        let deadline = Instant::now() + timeout;
        let mut cores = Vec::with_capacity(self.num_cores);
        while cores.len() < self.num_cores {
            let now = Instant::now();
            if now >= deadline {
                return Err(RuntimeError::TaskFailed(format!(
                    "Timed out collecting metrics: got {}/{} cores",
                    cores.len(),
                    self.num_cores
                )));
            }
            match receiver.recv_timeout(deadline.saturating_duration_since(now)) {
                Ok(snapshot) => cores.push(snapshot),
                Err(err) => {
                    return Err(RuntimeError::TaskFailed(format!(
                        "Metrics collection failed: {err}"
                    )));
                }
            }
        }
        cores.sort_by_key(|snapshot| snapshot.core_id);
        Ok(RuntimeMetrics { cores })
    }

    /// Wait for all cores to complete
    pub fn join(mut self) -> Result<()> {
        tracing::info!("Waiting for all cores to complete");

        let mut failures = Vec::new();
        for (core_id, handle) in self.join_handles.drain(..).enumerate() {
            match handle.join() {
                Ok(Ok(())) => {
                    tracing::debug!("Core {} completed successfully", core_id);
                }
                Ok(Err(e)) => {
                    tracing::error!("Core {} failed: {:?}", core_id, e);
                    failures.push(format!("core {core_id}: {e:?}"));
                }
                Err(e) => {
                    tracing::error!("Failed to join core {} thread: {:?}", core_id, e);
                    failures.push(format!("core {core_id} thread join failed: {e:?}"));
                }
            }
        }

        // Set state to terminated
        self.state
            .store(RuntimeState::Terminated as u8, Ordering::Release);

        if !failures.is_empty() {
            return Err(RuntimeError::TaskFailed(format!(
                "{} worker failures: {}",
                failures.len(),
                failures.join("; ")
            )));
        }
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
        if state == RuntimeState::Terminated as u8 || self.join_handles.is_empty() {
            return;
        }
        if state == RuntimeState::Running as u8 {
            tracing::warn!("Runtime dropped without explicit shutdown");
            let _ = self.shutdown();
        }
        // Worker owns raw eventfd numbers borrowed from `core_wakeup_fds`;
        // always join before those OwnedFd values are dropped.  Drop is a
        // control-plane blocking path, never a data-plane operation.
        for handle in self.join_handles.drain(..) {
            let _ = handle.join();
        }
        self.state
            .store(RuntimeState::Terminated as u8, Ordering::Release);
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

    #[test]
    #[cfg_attr(miri, ignore)]
    fn shutdown_with_timeout_waits_for_worker_exit_ack() {
        let runtime = MultiCoreRuntime::new(Some(1)).unwrap();
        runtime
            .shutdown_with_timeout(Duration::from_secs(1))
            .unwrap();
        match Arc::try_unwrap(runtime) {
            Ok(rt) => rt.join().unwrap(),
            Err(_) => panic!("Failed to get unique ownership of runtime"),
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn cancel_routes_to_owner_and_rejects_missing_shard() {
        let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
        let foreign = TaskId::from_key(crate::waker::TaskKey::new(1, 7, 0, 1));
        assert!(runtime.cancel_task(foreign).is_err());
        runtime.shutdown().unwrap();
        match Arc::try_unwrap(runtime) {
            Ok(rt) => rt.join().unwrap(),
            Err(_) => panic!("Failed to get unique ownership of runtime"),
        }
    }
}
