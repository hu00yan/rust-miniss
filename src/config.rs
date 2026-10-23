//! Configuration constants for the Miniss runtime
//!
//! This module contains tunable parameters that affect runtime behavior,
//! particularly around back-pressure and resource management.

use std::time::Duration;

/// Capacity shared by each shard's bounded cross-CPU inbox and I/O-retirement lane.
///
/// This is a compile-time capacity, not a runtime tuning knob. Keep its use in
/// sync with `MultiCoreRuntime` so the documented value matches actual backpressure.
pub const CROSS_CPU_CHANNEL_CAPACITY: usize = 4096;

/// Expected number of wakeups per timer expiration cycle
///
/// This optimizes the Vec allocation for expired timer wakers.
/// Most workloads have few simultaneous timer expirations.
pub const EXPECTED_WAKEUP_COUNT: usize = 16;

/// Requested default I/O park timeout in microseconds.
///
/// The selected backend uses this when no work is ready (`submit_and_wait` for
/// io_uring, `mio::Poll::poll` for the readiness backend). Mio applies a 1 ms
/// floor, so this value is not a latency guarantee across backends.
pub const DEFAULT_PARK_TIMEOUT_US: u64 = 100;

/// Default number of local tasks polled during one executor turn.
///
/// A task may wake itself while it is being polled.  Limiting the number of
/// polls before returning to the event loop prevents that task from starving
/// I/O completions, timers, messages, and shutdown processing.
pub const DEFAULT_TASK_POLL_BUDGET: usize = 64;

/// Optional completion batch limit used for controlled performance probes.
/// Zero means to retain the event-loop's phase-specific defaults.
pub fn io_drain_limit() -> Option<usize> {
    std::env::var("MINISS_IO_DRAIN_LIMIT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
}

/// Default control-plane metrics aggregation interval in milliseconds.
///
/// AGENTS.md MUST #6 requires aggregation outside the hot path.  One pass per
/// interval costs each shard exactly one bounded control-plane message.
pub const DEFAULT_METRICS_INTERVAL_MS: u64 = 1000;

/// Resolve the control-plane metrics aggregation interval from
/// `MINISS_METRICS_INTERVAL_MS`.
///
/// Read once when a runtime starts its aggregator thread, never from a worker.
/// Invalid or absent values fall back to [`DEFAULT_METRICS_INTERVAL_MS`].
pub fn metrics_interval() -> Duration {
    let interval_ms = std::env::var("MINISS_METRICS_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_METRICS_INTERVAL_MS);
    Duration::from_millis(interval_ms)
}

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
