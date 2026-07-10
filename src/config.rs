//! Configuration constants for the Miniss runtime
//!
//! This module contains tunable parameters that affect runtime behavior,
//! particularly around back-pressure and resource management.

use std::time::Duration;

/// Channel capacity for cross-CPU communication
///
/// This controls the bounded channel size used for sending messages between CPUs.
/// A larger value provides more buffering but uses more memory, while a smaller
/// value provides more back-pressure but may cause blocking under high load.
///
/// The default value of 1000 should be sufficient for most workloads while
/// providing adequate back-pressure when needed.
pub const CROSS_CPU_CHANNEL_CAPACITY: usize = 1000;

/// Default timeout for CPU thread operations (in milliseconds)
///
/// This controls how long CPU threads will wait for new messages when the
/// ready queue is empty. A smaller value provides more responsive shutdown
/// but uses more CPU cycles, while a larger value reduces CPU usage but
/// may delay shutdown.
pub const CPU_THREAD_TIMEOUT_MS: u64 = 10;

/// Initial capacity for task queue HashMap to reduce allocations
///
/// Pre-allocating the task queue reduces allocations during runtime.
/// This should be set based on expected concurrent task count per CPU.
pub const INITIAL_TASK_QUEUE_CAPACITY: usize = 128;

/// Expected number of wakeups per timer expiration cycle
///
/// This optimizes the Vec allocation for expired timer wakers.
/// Most workloads have few simultaneous timer expirations.
pub const EXPECTED_WAKEUP_COUNT: usize = 16;

/// Maximum expected tasks per CPU for utilization calculation
///
/// This is used for CPU utilization metrics and represents the
/// theoretical maximum number of tasks that could be queued per CPU.
/// Used for normalizing utilization percentages.
pub const MAX_TASKS: usize = 1024;

/// Default I/O park timeout in microseconds.
///
/// This bounds how long worker threads wait in `submit_and_wait` when the
/// ready queue is empty. Lower values favor p99 latency; higher values favor
/// throughput.
pub const DEFAULT_PARK_TIMEOUT_US: u64 = 100;

/// Default number of local tasks polled during one executor turn.
///
/// A task may wake itself while it is being polled.  Limiting the number of
/// polls before returning to the event loop prevents that task from starving
/// I/O completions, timers, messages, and shutdown processing.
pub const DEFAULT_TASK_POLL_BUDGET: usize = 64;

/// Resolve the I/O park timeout from `MINISS_PARK_TIMEOUT_US`.
///
/// This is read at runtime startup only. Invalid or absent values fall back to
/// `DEFAULT_PARK_TIMEOUT_US`.
pub fn park_timeout() -> Duration {
    let timeout_us = std::env::var("MINISS_PARK_TIMEOUT_US")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_PARK_TIMEOUT_US);
    Duration::from_micros(timeout_us)
}

/// Resolve the per-turn task poll budget from `MINISS_TASK_POLL_BUDGET`.
///
/// Invalid, zero, or absent values fall back to the safe default.  Read at
/// runtime/executor startup, never from the task polling hot path.
pub fn task_poll_budget() -> usize {
    std::env::var("MINISS_TASK_POLL_BUDGET")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_TASK_POLL_BUDGET)
}
