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

/// Default io_uring SQ/CQ ring entry count per shard.
pub const DEFAULT_URING_ENTRIES: u32 = 4096;

/// io_uring ring size for new backends.
///
/// Reads `MINISS_URING_ENTRIES`, clamped to `[64, 32768]`. This exists so
/// ring-size sweeps can be benchmarked without recompiling — a same-shape
/// comparison against another runtime must hold ring size equal, and it is
/// part of the documented confounders of the 2026-07 matrix.
pub fn uring_entries() -> u32 {
    std::env::var("MINISS_URING_ENTRIES")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .map(|value| value.clamp(64, 32768))
        .unwrap_or(DEFAULT_URING_ENTRIES)
}

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

/// Default control-plane metrics cadence in milliseconds.
///
/// AGENTS.md MUST #6 requires aggregation outside the hot path.  Workers push
/// their counters into single-writer slots at the event-loop boundary and
/// scrapes read those slots directly, so the runtime itself runs no timer on
/// this interval; it is the default cadence for embedder-side scrape or log
/// loops (see `examples/http_echo.rs`).
pub const DEFAULT_METRICS_INTERVAL_MS: u64 = 1000;

/// Resolve the control-plane metrics cadence from `MINISS_METRICS_INTERVAL_MS`.
///
/// Read once at runtime startup, never from a worker.  Invalid or absent
/// values fall back to [`DEFAULT_METRICS_INTERVAL_MS`].  See
/// [`metrics_publish_min_interval`] for the independent worker-side bound on
/// how often the published slot itself is refreshed.
pub fn metrics_interval() -> Duration {
    let interval_ms = std::env::var("MINISS_METRICS_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_METRICS_INTERVAL_MS);
    Duration::from_millis(interval_ms)
}

/// Default minimum interval between a worker's loop-boundary metrics
/// publications, in milliseconds.
pub const DEFAULT_METRICS_PUBLISH_MIN_MS: u64 = 10_000;

/// Resolve the minimum interval between a worker's loop-boundary metrics
/// publications from `MINISS_METRICS_PUBLISH_MIN_MS`.
///
/// Valid values are milliseconds including `0` (publish on every loop turn);
/// invalid or absent values fall back to [`DEFAULT_METRICS_PUBLISH_MIN_MS`].
/// Read once at worker startup, never from the event loop.
///
/// Relationship to [`metrics_interval`]: that knob is the embedder-side
/// scrape/log cadence reference — the runtime itself runs no timer on it —
/// while this knob bounds how often the owning worker republishes its
/// single-writer slot at the event-loop boundary. The two are independent by
/// design: a scrape may therefore observe a slot snapshot up to this interval
/// old, which is safe because every published snapshot carries cumulative
/// monotonic counters (staleness costs freshness, never truth), and the
/// worker's final publication at shutdown always runs regardless of this
/// knob.
pub fn metrics_publish_min_interval() -> Duration {
    std::env::var("MINISS_METRICS_PUBLISH_MIN_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(DEFAULT_METRICS_PUBLISH_MIN_MS))
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

#[cfg(test)]
mod config_tests {
    use super::uring_entries;

    #[test]
    fn uring_entries_clamps_and_falls_back() {
        // SAFETY: tests run single-threaded per process env; set_var is safe
        // here because no other thread reads this variable concurrently.
        unsafe {
            std::env::remove_var("MINISS_URING_ENTRIES");
        }
        assert_eq!(uring_entries(), super::DEFAULT_URING_ENTRIES);

        for (raw, expected) in [
            ("1024", 1024),
            ("63", 64),
            ("999999", 32768),
            ("not-a-number", super::DEFAULT_URING_ENTRIES),
            ("0", 64),
        ] {
            // SAFETY: as above.
            unsafe {
                std::env::set_var("MINISS_URING_ENTRIES", raw);
            }
            assert_eq!(uring_entries(), expected, "raw={raw}");
        }
        // SAFETY: as above.
        unsafe {
            std::env::remove_var("MINISS_URING_ENTRIES");
        }
    }
}
