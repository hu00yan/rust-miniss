//! Single-threaded executor with panic isolation
//!
//! This module provides a basic single-threaded async executor that can
//! run futures to completion. The executor implements robust error handling
//! by catching panics at the polling level, ensuring that panicked tasks
//! don't bring down the entire runtime.
//!
//! ## Panic Handling
//!
//! Every task poll operation is wrapped with `std::panic::catch_unwind` to provide
//! isolation between tasks. When a task panics:
//!
//! 1. The panic is caught and logged
//! 2. The task is marked as completed (removed from the task queue)
//! 3. Other tasks continue to execute normally
//! 4. The waker for the panicked task is automatically dropped
//!
//! JoinHandles return `TaskResult<T>` which is `Result<T, TaskError>` where
//! `TaskError::Panic` contains the panic payload for analysis.

use crossbeam_queue::ArrayQueue;
use std::cell::RefCell;
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::cpu::CpuIoState;
use crate::error::{Result, RuntimeError};
use crate::task::{DetachedTaskFuture, JoinHandle, JoinTaskFuture, Task};
use crate::timer::TimerWheel;
use crate::waker::{ReadyQueue, TaskArena, TaskId};

type IoCompletion = (
    crate::io::IoToken,
    std::result::Result<crate::io::CompletionKind, crate::io::IoError>,
);

/// A single-threaded async runtime.
///
/// `Runtime` owns its I/O driver and timer wheel directly. It is not `Send`
/// or `Sync` and must only be used from the thread that created it. The
/// `RefCell` field is what makes the type non-`Send` (rm-01).
///
/// ```compile_fail
/// fn requires_send<T: Send>(_: T) {}
/// requires_send(rust_miniss::Runtime::new());
/// ```
pub struct Runtime {
    executor: RefCell<Executor>, // Changed from Mutex to RefCell (single-threaded)
    io_state: RefCell<CpuIoState>,
    timer_wheel: RefCell<TimerWheel>,
}

// SAFETY: Runtime owns shard-local driver/timer state behind `RefCell` and
// must never cross a thread boundary. See rm-01 work package A. `Runtime` is
// `!Send + !Sync` because it contains `RefCell<Executor>` (and historically a
// public `shutdown_flag` that implied unimplemented lifecycle semantics, now
// removed). The `tls_context_restores_after_nesting_and_panic` and
// `runtime_move_and_reentry_keeps_tls_valid` tests exercise the scoped guard.
// A `static_assertions::assert_not_impl_any!(Runtime: Send, Sync)` is intentionally
// omitted: the macro hits a type-inference ambiguity on this concrete type in
// this crate, and the `RefCell` field already makes the type non-`Send` by
// construction.

impl Runtime {
    /// Create a new runtime
    pub fn new() -> Self {
        Self {
            executor: RefCell::new(Executor::new()), // Changed from Mutex::new
            io_state: RefCell::new(CpuIoState::default()),
            timer_wheel: RefCell::new(TimerWheel::default()),
        }
    }

    /// Run a future to completion
    pub fn block_on<F>(&self, future: F) -> F::Output
    // arch-audit: allow control-plane runtime API
    where
        F: Future,
    {
        use crate::cpu::RuntimeContextGuard;

        let io_state = self.io_state.borrow_mut();

        // Pin the future to the stack
        let mut future = Box::pin(future);
        let mut timer_wheel = self.timer_wheel.borrow_mut();
        // SAFETY: Runtime owns both RefCell values for its full lifetime;
        // borrows remain active until context guard and block_on return.
        let _runtime_context =
            unsafe { RuntimeContextGuard::enter(&io_state, &mut timer_wheel, None) };
        let mut ready_timers = Vec::with_capacity(crate::config::EXPECTED_WAKEUP_COUNT);
        let mut io_completions: Vec<IoCompletion> =
            Vec::with_capacity(crate::config::EXPECTED_WAKEUP_COUNT * 4);

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
            // Set current_task_id so IoFuture can register for inline completion.
            // In `Runtime::block_on()`, there's no CpuCore event loop doing inline
            // completion processing — completions are handled in the Pending branch
            // below. But setting it here is still correct: IoFuture::poll() will
            // store_pending_task which we don't use in this path, and it'll also
            // store_waker which IS used. The task_id is cleared after poll returns.
            crate::cpu::set_current_task_id(Some(crate::waker::TaskId(0)));
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => {
                    crate::cpu::set_current_task_id(None);
                    return output;
                }
                Poll::Pending => {
                    crate::cpu::set_current_task_id(None);
                    let mut made_progress = self.executor.borrow_mut().tick();

                    // Check for I/O completions
                    // Flush all queued SQEs before polling for completions
                    io_state.io_backend.flush();

                    // Drain I/O completions
                    io_completions.clear();
                    if let Poll::Ready(()) = io_state
                        .io_backend
                        .drain_completions_into(&mut io_completions)
                    {
                        if !io_completions.is_empty() {
                            // SAFETY: Single-threaded access.
                            for (token, result) in io_completions.drain(..) {
                                let is_accept =
                                    matches!(result, Ok(crate::io::CompletionKind::Accept { .. }))
                                        && io_state.io_backend.supports_multishot_accept();
                                if is_accept {
                                    // SAFETY: Single-threaded executor owns
                                    // CpuIoState while polling completions.
                                    if let Some(waker) = unsafe { io_state.take_waker(token) } {
                                        waker.wake();
                                    }
                                    continue;
                                }
                                // SAFETY: Completion token came from this
                                // executor's backend and is consumed once.
                                if let Some(waker) = unsafe {
                                    io_state.insert_completed_and_take_waker(token, result)
                                } {
                                    waker.wake();
                                }
                            }
                            made_progress = true;
                        }
                    }

                    if timer_wheel.has_pending() {
                        timer_wheel.expire(std::time::Instant::now(), &mut ready_timers);
                        if !ready_timers.is_empty() {
                            made_progress = true;
                            for waker in ready_timers.drain(..) {
                                waker.wake();
                            }
                        }
                    }

                    // Drive tasks spawned by the accept loop / connection handlers
                    // once more after I/O and timer work. This keeps spawned work
                    // from waiting until the next outer poll when the main future
                    // has already made progress.
                    if self.executor.borrow_mut().tick() {
                        made_progress = true;
                    }

                    if made_progress {
                        continue;
                    }

                    // Park using the I/O backend. With DEFER_TASKRUN, completions are
                    // only posted to the CQ ring when we enter the kernel via
                    // submit_and_wait. park() handles this correctly.
                    io_state.io_backend.park(crate::config::park_timeout());

                    // Drain any completions that arrived during park.
                    io_completions.clear();
                    if let Poll::Ready(()) = io_state
                        .io_backend
                        .drain_completions_into(&mut io_completions)
                    {
                        if !io_completions.is_empty() {
                            for (token, result) in io_completions.drain(..) {
                                let is_accept =
                                    matches!(result, Ok(crate::io::CompletionKind::Accept { .. }))
                                        && io_state.io_backend.supports_multishot_accept();
                                if is_accept {
                                    // SAFETY: Single-threaded executor owns
                                    // CpuIoState while polling completions.
                                    if let Some(waker) = unsafe { io_state.take_waker(token) } {
                                        waker.wake();
                                    }
                                    continue;
                                }
                                // SAFETY: Completion token came from this
                                // executor's backend and is consumed once.
                                if let Some(waker) = unsafe {
                                    io_state.insert_completed_and_take_waker(token, result)
                                } {
                                    waker.wake();
                                }
                            }
                            continue;
                        }
                    }
                }
            }
        }
    }

    /// Spawn a new task
    pub fn spawn<F, T>(&self, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.try_spawn(future)
            .expect("executor task arena capacity exhausted")
    }

    /// Fallible spawn variant. Capacity exhaustion is reported before the
    /// future is admitted, so callers can apply backpressure instead of
    /// aborting the owner thread.
    pub fn try_spawn<F, T>(&self, future: F) -> Result<JoinHandle<T>>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.executor.borrow_mut().try_spawn(future)
    }

    /// Spawn a fire-and-forget task without creating a join handle.
    pub fn spawn_detached<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.try_spawn_detached(future)
            .expect("executor task arena capacity exhausted");
    }

    /// Fallible detached spawn variant.
    pub fn try_spawn_detached<F>(&self, future: F) -> Result<TaskId>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.executor.borrow_mut().try_spawn_detached(future)
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

/// The core executor that manages task scheduling
pub struct Executor {
    /// Fixed-capacity generational storage.  Ready records carry the full
    /// key, so an old wake can never address a replacement task.
    tasks: TaskArena<Task>,
    ready_queue: ReadyQueue,
    active_tasks: usize,
    park_timeout: std::time::Duration,
    /// Maximum tasks polled before returning control to the event loop.
    poll_budget: usize,
}

impl Executor {
    /// Create a new executor
    pub fn new() -> Self {
        Self::with_park_timeout(crate::config::park_timeout())
    }

    /// Create a new executor with explicit idle park timeout.
    pub fn with_park_timeout(park_timeout: std::time::Duration) -> Self {
        Self::with_park_timeout_and_budget(park_timeout, crate::config::task_poll_budget())
    }

    /// Create an executor with explicit park timeout and per-turn poll budget.
    pub fn with_park_timeout_and_budget(
        park_timeout: std::time::Duration,
        poll_budget: usize,
    ) -> Self {
        Self::with_task_capacity_and_budget(65_536, park_timeout, poll_budget)
    }

    /// Create an executor with an explicit bounded task capacity.
    pub fn with_task_capacity(capacity: usize) -> Self {
        Self::with_task_capacity_and_budget(
            capacity,
            crate::config::park_timeout(),
            crate::config::task_poll_budget(),
        )
    }

    fn with_task_capacity_and_budget(
        capacity: usize,
        park_timeout: std::time::Duration,
        poll_budget: usize,
    ) -> Self {
        let capacity = capacity.max(1);
        Self {
            tasks: TaskArena::with_capacity(crate::waker::next_runtime_generation(), 0, capacity),
            ready_queue: Arc::new(ArrayQueue::new(capacity)),
            active_tasks: 0,
            park_timeout,
            poll_budget: poll_budget.max(1),
        }
    }

    /// Run a future to completion
    pub fn block_on<F>(&self, future: F) -> F::Output
    // arch-audit: allow control-plane runtime API
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
        self.try_spawn(future)
            .expect("executor task arena capacity exhausted")
    }

    /// Fallible task admission for callers that need bounded backpressure.
    pub fn try_spawn<F, T>(&mut self, future: F) -> crate::error::Result<JoinHandle<T>>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let key = self.tasks.reserve().ok_or_else(|| {
            RuntimeError::TaskFailed("executor task arena capacity exhausted".to_string())
        })?;
        let task_id = TaskId::from_key(key);

        let (handle, completer) = JoinHandle::new(task_id);
        let wrapped_future = JoinTaskFuture::new(future, completer);

        // Create the task
        let mut task = Task::new(task_id, wrapped_future);

        // Add to the generational arena and ready queue.
        task.ensure_waker(self.ready_queue.clone());
        task.schedule();
        assert!(self.tasks.insert(key, task));
        self.active_tasks += 1;

        Ok(handle)
    }

    /// Spawn a task without tracking a join handle.
    pub fn spawn_detached<F>(&mut self, future: F) -> TaskId
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.try_spawn_detached(future)
            .expect("executor task arena capacity exhausted")
    }

    /// Fallible detached task admission.
    pub fn try_spawn_detached<F>(&mut self, future: F) -> crate::error::Result<TaskId>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let key = self.tasks.reserve().ok_or_else(|| {
            RuntimeError::TaskFailed("executor task arena capacity exhausted".to_string())
        })?;
        let task_id = TaskId::from_key(key);

        let mut task = Task::new(task_id, DetachedTaskFuture::new(future));
        task.ensure_waker(self.ready_queue.clone());
        task.schedule();
        assert!(self.tasks.insert(key, task));
        self.active_tasks += 1;

        Ok(task_id)
    }

    /// Run all ready tasks once
    pub fn tick(&mut self) -> bool {
        let mut made_progress = false;
        let mut polled = 0;

        while polled < self.poll_budget {
            let Some(task_id) = self.ready_queue.pop() else {
                break;
            };
            polled += 1;
            let key = task_id.key();
            let poll_result = {
                let Some(task) = self.tasks.get_mut(key) else {
                    continue;
                };
                if task.id() != task_id {
                    continue;
                }
                // Reuse the task-local waker cache so the hot path does not
                // allocate or clone a new waker on every poll.
                crate::cpu::set_current_task_id(Some(task_id));
                let result = task.poll_with_cached_waker();
                crate::cpu::set_current_task_id(None);
                result
            };

            match poll_result {
                Poll::Ready(()) => {
                    self.tasks.remove(key);
                    self.active_tasks = self.active_tasks.saturating_sub(1);
                    made_progress = true;
                }
                Poll::Pending => {
                    made_progress = true;
                }
            }
        }

        // A wake can arrive exactly while the bounded queue is full. Retry
        // sticky overflow markers after consuming this turn's records so a
        // one-shot wake cannot strand a pending task forever.
        for task in self.tasks.iter().flatten() {
            made_progress |= task.retry_overflowed_wake();
        }

        made_progress
    }

    /// Run the executor until all tasks complete
    pub fn run(&mut self) {
        while self.active_tasks > 0 {
            if !self.tick() {
                // No progress made; park the thread briefly to avoid busy loop
                std::thread::park_timeout(self.park_timeout);
            }
        }
    }

    /// Get the number of active tasks
    pub fn task_count(&self) -> usize {
        self.active_tasks
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
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_runtime_creation() {
        let _runtime = Runtime::new(); // arch-audit: allow unit test runtime creation
                                       // Runtime should be created successfully
    }

    #[test]
    fn try_spawn_reports_admission_result_without_panicking() {
        let runtime = Runtime::new();
        assert!(runtime.try_spawn(async { 1u8 }).is_ok());
    }

    #[test]
    fn try_spawn_rejects_full_bounded_arena() {
        let mut executor = Executor::with_task_capacity(1);
        assert!(executor.try_spawn(async {}).is_ok());
        assert!(executor.try_spawn(async {}).is_err());
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_block_on_immediate() {
        let runtime = Runtime::new();
        let result = runtime.block_on(async { 42 });
        assert_eq!(result, 42);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
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
    fn tls_context_restores_after_nesting_and_panic() {
        let outer = Runtime::new();
        outer.block_on(async {
            let outer_state = crate::cpu::with_io_state(|state| state as *const _);
            let inner = Runtime::new();

            let inner_state =
                inner.block_on(async { crate::cpu::with_io_state(|state| state as *const _) });
            assert_ne!(outer_state, inner_state);
            assert_eq!(
                outer_state,
                crate::cpu::with_io_state(|state| state as *const _)
            );

            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                inner.block_on(async { panic!("nested runtime panic") })
            }));
            assert!(panicked.is_err());
            assert_eq!(
                outer_state,
                crate::cpu::with_io_state(|state| state as *const _)
            );
        });

        assert!(crate::cpu::try_with_io_state(|_| ()).is_none());
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
            completed_clone.store(true, Ordering::Release);
            42
        });

        assert_eq!(executor.task_count(), 1);

        // Tick should run the task
        assert!(executor.tick());
        assert!(completed.load(Ordering::Acquire));
        assert_eq!(executor.task_count(), 0); // Task completed
    }

    #[test]
    fn poll_budget_bounds_self_waking_task_and_runs_peer() {
        use std::task::Poll;

        struct SelfWaking {
            polls: Arc<AtomicU32>,
        }

        impl Future for SelfWaking {
            type Output = ();

            fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                self.polls.fetch_add(1, Ordering::Relaxed);
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }

        let mut executor =
            Executor::with_park_timeout_and_budget(std::time::Duration::from_millis(1), 1);
        let self_polls = Arc::new(AtomicU32::new(0));
        executor.spawn(SelfWaking {
            polls: self_polls.clone(),
        });

        let peer_ran = Arc::new(AtomicBool::new(false));
        let peer_ran_clone = peer_ran.clone();
        executor.spawn(async move {
            peer_ran_clone.store(true, Ordering::Release);
        });

        // One tick may poll only one ready task, despite the first task
        // continually waking itself. The peer must run on the next turn.
        assert!(executor.tick());
        assert_eq!(self_polls.load(Ordering::Acquire), 1);
        assert!(!peer_ran.load(Ordering::Acquire));
        assert!(executor.tick());
        assert!(peer_ran.load(Ordering::Acquire));
        assert_eq!(self_polls.load(Ordering::Acquire), 1);
        assert_eq!(executor.task_count(), 1);
    }

    #[test]
    fn test_executor_run() {
        let mut executor = Executor::new();
        let counter = Arc::new(AtomicU32::new(0));

        // Spawn multiple tasks
        for i in 0..5 {
            let counter_clone = counter.clone();
            executor.spawn(async move {
                counter_clone.fetch_add(i, Ordering::Relaxed);
            });
        }

        assert_eq!(executor.task_count(), 5);

        // Run all tasks to completion
        executor.run();

        assert_eq!(executor.task_count(), 0);
        assert_eq!(counter.load(Ordering::Acquire), 1 + 2 + 3 + 4);
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
        let (tx, rx) = std::sync::mpsc::sync_channel(3);

        // Spawn tasks that complete in different "rounds"
        for i in 0..3 {
            let tx = tx.clone();
            executor.spawn(async move {
                tx.send(i).unwrap();
            });
        }
        drop(tx);

        executor.run();

        let final_results: Vec<_> = rx.try_iter().collect();
        assert_eq!(final_results.len(), 3);
        // Results might be in any order due to task scheduling
        assert!(final_results.contains(&0));
        assert!(final_results.contains(&1));
        assert!(final_results.contains(&2));
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
            flag_clone.store(true, Ordering::Release);
        });

        // This will panic if the panic is not caught
        executor.run();

        assert!(flag.load(Ordering::Acquire), "Task B should have run");
    }

    #[test]
    fn test_multiple_panics() {
        let mut executor = Executor::new();
        let completed_count = Arc::new(AtomicU32::new(0));

        for i in 0..10 {
            let completed_count_clone = completed_count.clone();
            if i % 2 == 0 {
                executor.spawn(async move {
                    panic!("boom {i}");
                });
            } else {
                executor.spawn(async move {
                    completed_count_clone.fetch_add(1, Ordering::Relaxed);
                });
            }
        }

        executor.run();

        assert_eq!(
            completed_count.load(Ordering::Acquire),
            5,
            "All non-panicking tasks should complete"
        );
    }

    // Additional tests for regression prevention

    #[test]
    fn test_string_literals_in_asserts() {
        // Test that string literals in assert messages work correctly
        let mut executor = Executor::new();
        let success = Arc::new(AtomicBool::new(false));
        let success_clone = success.clone();

        executor.spawn(async move {
            success_clone.store(true, Ordering::Release);
        });

        executor.run();

        // This should not cause compilation errors with string literals
        assert!(
            success.load(Ordering::Acquire),
            "Task should have completed successfully"
        );
    }

    #[test]
    fn test_complex_assert_messages() {
        // Test complex assert messages with various characters
        let mut executor = Executor::new();
        let counter = Arc::new(AtomicU32::new(0));

        for _i in 0..3 {
            let counter_clone = counter.clone();
            executor.spawn(async move {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            });
        }

        executor.run();

        assert_eq!(
            counter.load(Ordering::Acquire),
            3,
            "All tasks should complete: expected 3, got {}",
            counter.load(Ordering::Acquire)
        );
    }

    #[test]
    fn test_vector_contains_operations() {
        // Specifically test the vector contains operations that were fixed
        let mut executor = Executor::new();
        let (tx, rx) = std::sync::mpsc::sync_channel(3);

        // Spawn tasks with specific values
        for i in [42, 100, 255] {
            let tx = tx.clone();
            executor.spawn(async move {
                tx.send(i).unwrap();
            });
        }
        drop(tx);

        executor.run();

        let final_results: Vec<_> = rx.try_iter().collect();
        assert_eq!(final_results.len(), 3);

        // Test all the contains operations that were previously broken
        assert!(final_results.contains(&42), "Should contain 42");
        assert!(final_results.contains(&100), "Should contain 100");
        assert!(final_results.contains(&255), "Should contain 255");

        // Ensure they don't contain values we didn't add
        assert!(!final_results.contains(&0), "Should not contain 0");
        assert!(!final_results.contains(&999), "Should not contain 999");
    }

    #[test]
    fn test_task_isolation_with_detailed_messages() {
        // Test task isolation with detailed error messages
        let mut executor = Executor::new();
        let completed_tasks = Arc::new(AtomicU32::new(0));
        let panic_tasks = Arc::new(AtomicU32::new(0));

        // Mix of panicking and non-panicking tasks
        for i in 0..6 {
            let completed_clone = completed_tasks.clone();
            let panic_clone = panic_tasks.clone();

            if i % 3 == 0 {
                // Panicking task
                executor.spawn(async move {
                    panic_clone.fetch_add(1, Ordering::Relaxed);
                    panic!("intentional panic in task {i}");
                });
            } else {
                // Normal task
                executor.spawn(async move {
                    completed_clone.fetch_add(1, Ordering::Relaxed);
                });
            }
        }

        executor.run();

        assert_eq!(
            completed_tasks.load(Ordering::Acquire),
            4,
            "Expected 4 non-panicking tasks to complete"
        );
        assert_eq!(
            panic_tasks.load(Ordering::Acquire),
            2,
            "Expected 2 panicking tasks to run before panicking"
        );
    }

    #[test]
    fn test_runtime_spawn_and_completion() {
        // Test the Runtime struct's spawn method with proper string handling
        // Note: In this single-threaded executor, we need to use the Executor directly
        // to run spawned tasks, as Runtime's block_on doesn't execute the task queue
        let mut executor = Executor::new();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);

        let handle = executor.spawn(async move {
            tx.send("task completed".to_string()).unwrap();
            "done"
        });

        // Run the executor to complete the task
        executor.run();

        // Check that the task completed
        assert!(handle.is_finished());

        let final_result: Vec<_> = rx.try_iter().collect();
        assert!(
            final_result.contains(&"task completed".to_string()),
            "Runtime spawn should complete successfully"
        );
    }

    // rm-01: moving a `Runtime` and re-entering `block_on` must not invalidate
    // the scoped TLS addresses installed by `RuntimeContextGuard`.
    #[test]
    fn runtime_move_and_reentry_keeps_tls_valid() {
        let runtime = Runtime::new();
        // Move the runtime through a scope boundary.
        let runtime = { runtime };
        // First entry installs TLS and drives I/O.
        let first =
            runtime.block_on(async { crate::cpu::with_io_state(|state| state as *const _) });
        // Second entry must install the *same* address (same stack owner).
        let second =
            runtime.block_on(async { crate::cpu::with_io_state(|state| state as *const _) });
        assert_eq!(first, second, "TLS address must be stable across re-entry");
        // After the runtime is dropped, no context remains installed.
        drop(runtime);
        assert!(crate::cpu::try_with_io_state(|_| ()).is_none());
    }

    // rm-01: a panic inside `block_on` must leave no stale TLS, and a fresh
    // runtime must be able to install context and run afterwards.
    #[test]
    fn panic_in_block_on_allows_fresh_io_afterwards() {
        let runtime = Runtime::new();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async {
                crate::cpu::with_io_state(|_| ());
                panic!("boom");
            })
        }));
        assert!(panicked.is_err());
        // The guard's Drop restored the previous (null) context.
        assert!(crate::cpu::try_with_io_state(|_| ()).is_none());
        // A new runtime can still install context and run.
        let runtime2 = Runtime::new();
        let installed =
            runtime2.block_on(async { crate::cpu::try_with_io_state(|_| true).unwrap_or(false) });
        assert!(installed, "fresh runtime must install its own context");
        // And it clears it again on exit.
        assert!(crate::cpu::try_with_io_state(|_| ()).is_none());
    }
}
