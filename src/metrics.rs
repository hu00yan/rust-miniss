//! Control-plane metrics publication and Prometheus text exposition.
//!
//! Publication is worker-side push at the event-loop boundary: every worker
//! copies its already-maintained shard-local counters into its own
//! single-writer slot ([`crate::multicore::CoreMetricsSlot`]) once per loop
//! turn, at the same boundary as the ready-queue hint. A scrape assembles the
//! runtime-wide view inline on the scraping thread by reading the per-shard
//! slots directly; the merged total is the fold of the per-shard values in the
//! same view. There is no sampler thread, no periodic pass, no
//! `CollectMetrics` message, no reply channel, and no worker work on the
//! control plane's clock — at steady state a runtime owns exactly its worker
//! threads.
//!
//! This is deliberately not a process-global registry: AGENTS.md MUST NOT #4
//! forbids the statics such a registry needs, and slots can only belong to the
//! shards of one runtime, so two runtimes in one process would merge into a
//! meaningless series set. The metrics control plane is owned by one
//! [`MultiCoreRuntime`](crate::multicore::MultiCoreRuntime).
//!
//! Coherence between the per-series values of one shard snapshot is provided
//! by the slot's seqlock-style version (see [`crate::multicore::CoreMetricsSlot`]);
//! coherence of the runtime-wide fold is by construction, because the fold is
//! computed from one read of each slot on one thread. AGENTS.md MUST #6 holds
//! by a stronger construction than the message-passing design it replaced:
//! publication happens outside the request path (between loop turns, stores of
//! counters the worker already maintains) and aggregation happens entirely on
//! the reader's thread.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use crate::multicore::{CoreMetricsSnapshot, RuntimeMetrics};

/// How one shard-local series merges into the runtime-wide series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merge {
    /// Monotonic counter: the runtime-wide value is the arithmetic sum.
    Sum,
    /// High-water mark: the runtime-wide value is the largest shard value.
    /// Summing a maximum is meaningless, so these are `max`, never `+`.
    Max,
}

/// The `# TYPE` a series is advertised with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricType {
    Counter,
    Gauge,
}

impl MetricType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
        }
    }
}

/// One exported metric and the projection that reads it off a shard snapshot.
#[derive(Clone, Copy)]
pub struct SeriesDescriptor {
    /// Fully qualified Prometheus metric name.
    pub name: &'static str,
    /// `# HELP` text.
    pub help: &'static str,
    /// `# TYPE` kind.
    pub metric_type: MetricType,
    /// How shard values merge into the unlabelled total series.
    pub merge: Merge,
    read: fn(&CoreMetricsSnapshot) -> u64,
}

impl SeriesDescriptor {
    /// Project this series out of one shard snapshot.
    pub fn read(&self, shard: &CoreMetricsSnapshot) -> u64 {
        (self.read)(shard)
    }

    /// Fold shard values into the runtime-wide value for this series.
    pub fn merge_all<'a>(&self, shards: impl Iterator<Item = &'a CoreMetricsSnapshot>) -> u64 {
        let mut total = 0u64;
        for shard in shards {
            let value = self.read(shard);
            total = match self.merge {
                Merge::Sum => total.saturating_add(value),
                Merge::Max => total.max(value),
            };
        }
        total
    }
}

impl std::fmt::Debug for SeriesDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeriesDescriptor")
            .field("name", &self.name)
            .field("metric_type", &self.metric_type)
            .field("merge", &self.merge)
            .finish()
    }
}

const fn series(
    name: &'static str,
    help: &'static str,
    metric_type: MetricType,
    merge: Merge,
    read: fn(&CoreMetricsSnapshot) -> u64,
) -> SeriesDescriptor {
    SeriesDescriptor {
        name,
        help,
        metric_type,
        merge,
        read,
    }
}

/// Every field of [`CoreMetricsSnapshot`] except `core_id`, which becomes the
/// `shard` label rather than a value.
///
/// Order matches the struct declaration.  `snapshot_fields_and_series_stay_in_sync`
/// destructures the struct exhaustively, so adding a field without a matching
/// entry here is a compile error in that test.
const SERIES: &[SeriesDescriptor] = &[
    series(
        "miniss_local_task_count",
        "Tasks currently owned by the shard.",
        MetricType::Gauge,
        Merge::Sum,
        |s| s.local_task_count as u64,
    ),
    series(
        "miniss_ready_queue_tasks",
        "Tasks currently runnable on the shard.",
        MetricType::Gauge,
        Merge::Sum,
        |s| s.ready_queue_len as u64,
    ),
    series(
        "miniss_max_ready_queue_tasks",
        "High-water mark of the shard ready queue.",
        MetricType::Gauge,
        Merge::Max,
        |s| s.max_ready_queue_len as u64,
    ),
    series(
        "miniss_tasks_spawned_total",
        "Tasks admitted on the shard.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.tasks_spawned,
    ),
    series(
        "miniss_tasks_completed_total",
        "Tasks that reached a terminal state on the shard.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.tasks_completed,
    ),
    series(
        "miniss_tasks_polled_total",
        "Task polls performed on the shard.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.tasks_polled,
    ),
    series(
        "miniss_io_completions_total",
        "IO completions delivered to the shard.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.io_completions,
    ),
    series(
        "miniss_messages_processed_total",
        "Cross-core messages drained by the shard.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.messages_processed,
    ),
    series(
        "miniss_message_batches_total",
        "Message drain batches run by the shard.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.message_batches,
    ),
    series(
        "miniss_max_message_inbox_messages",
        "High-water mark of the shard message inbox, in messages.",
        MetricType::Gauge,
        Merge::Max,
        |s| s.max_message_inbox_len as u64,
    ),
    series(
        "miniss_busy_rounds_total",
        "Inner event-loop rounds that did work.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.busy_rounds,
    ),
    series(
        "miniss_flushes_total",
        "Submission flushes issued by the shard.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.flushes,
    ),
    series(
        "miniss_flush_duration_microseconds_total",
        "Cumulative time spent flushing submissions, in microseconds.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.flush_total_us,
    ),
    series(
        "miniss_flush_duration_microseconds",
        "Slowest single submission flush, in microseconds.",
        MetricType::Gauge,
        Merge::Max,
        |s| s.max_flush_us,
    ),
    series(
        "miniss_park_flushes_total",
        "Flushes performed inside the park syscall.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.park_flushes,
    ),
    series(
        "miniss_park_flush_duration_microseconds_total",
        "Cumulative flush time inside the park syscall, in microseconds.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.park_flush_total_us,
    ),
    series(
        "miniss_park_flush_duration_microseconds",
        "Slowest flush performed inside the park syscall, in microseconds.",
        MetricType::Gauge,
        Merge::Max,
        |s| s.max_park_flush_us,
    ),
    series(
        "miniss_io_completion_batches_total",
        "IO completion batches drained by the shard.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.io_completion_batches,
    ),
    series(
        "miniss_io_completion_duration_nanoseconds_total",
        "Cumulative time spent draining IO completions, in nanoseconds.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.io_completion_total_ns,
    ),
    series(
        "miniss_io_completion_batch_duration_nanoseconds",
        "Slowest IO completion batch, in nanoseconds.",
        MetricType::Gauge,
        Merge::Max,
        |s| s.max_io_completion_batch_ns,
    ),
    series(
        "miniss_max_io_completion_batch_items",
        "Largest IO completion batch, in items.",
        MetricType::Gauge,
        Merge::Max,
        |s| s.max_io_completion_batch_len as u64,
    ),
    series(
        "miniss_inline_poll_duration_nanoseconds_total",
        "Cumulative time spent polling inline completions, in nanoseconds.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.inline_poll_total_ns,
    ),
    series(
        "miniss_inline_poll_duration_nanoseconds",
        "Slowest inline completion poll, in nanoseconds.",
        MetricType::Gauge,
        Merge::Max,
        |s| s.max_inline_poll_ns,
    ),
    series(
        "miniss_wake_duration_nanoseconds_total",
        "Cumulative time spent waking tasks, in nanoseconds.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.wake_total_ns,
    ),
    series(
        "miniss_wake_duration_nanoseconds",
        "Slowest single wake, in nanoseconds.",
        MetricType::Gauge,
        Merge::Max,
        |s| s.max_wake_ns,
    ),
    series(
        "miniss_parks_total",
        "Times the shard parked waiting for IO.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.parks,
    ),
    series(
        "miniss_park_timeouts_total",
        "Parks that returned without a completion.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.park_timeouts,
    ),
    series(
        "miniss_park_completions_total",
        "Completions harvested by parking.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.park_completions,
    ),
    series(
        "miniss_park_duration_microseconds_total",
        "Cumulative time parked, in microseconds.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.park_total_us,
    ),
    series(
        "miniss_park_duration_microseconds",
        "Slowest single park, in microseconds.",
        MetricType::Gauge,
        Merge::Max,
        |s| s.max_park_us,
    ),
    series(
        "miniss_timer_expirations_total",
        "Timer entries that reached their deadline.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.timer_expirations,
    ),
    series(
        "miniss_inline_completion_hits_total",
        "Completions resolved without leaving the poll loop.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.inline_completion_hits,
    ),
    series(
        "miniss_waker_wakeups_total",
        "Waker wake calls observed by the shard.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.waker_wakeups,
    ),
    series(
        "miniss_quic_udp_tx_packets_total",
        "QUIC UDP datagrams transmitted by connections accepted on the shard.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.quic_udp_tx_packets,
    ),
    series(
        "miniss_quic_udp_rx_packets_total",
        "QUIC UDP datagrams received by connections accepted on the shard.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.quic_udp_rx_packets,
    ),
    series(
        "miniss_quic_handshakes_accepted_total",
        "QUIC handshakes accepted by endpoints on the shard.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.quic_handshakes_accepted,
    ),
    series(
        "miniss_quic_handshakes_refused_total",
        "QUIC handshakes refused by endpoints on the shard.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.quic_handshakes_refused,
    ),
    series(
        "miniss_quic_connections_active",
        "Live QUIC connections tracked by acceptors on the shard.",
        MetricType::Gauge,
        Merge::Max,
        |s| s.quic_connections_active as u64,
    ),
];

/// Previous exposition names accepted only by [`MetricsView::value`] lookups.
/// The exporter emits only the canonical names in `SERIES`.
const LEGACY_NAME_ALIASES: &[(&str, &str)] = &[
    ("miniss_ready_queue_len", "miniss_ready_queue_tasks"),
    ("miniss_max_ready_queue_len", "miniss_max_ready_queue_tasks"),
    (
        "miniss_max_message_inbox_len",
        "miniss_max_message_inbox_messages",
    ),
    (
        "miniss_flush_total_us",
        "miniss_flush_duration_microseconds_total",
    ),
    ("miniss_max_flush_us", "miniss_flush_duration_microseconds"),
    (
        "miniss_park_flush_total_us",
        "miniss_park_flush_duration_microseconds_total",
    ),
    (
        "miniss_max_park_flush_us",
        "miniss_park_flush_duration_microseconds",
    ),
    (
        "miniss_io_completion_total_ns",
        "miniss_io_completion_duration_nanoseconds_total",
    ),
    (
        "miniss_max_io_completion_batch_ns",
        "miniss_io_completion_batch_duration_nanoseconds",
    ),
    (
        "miniss_max_io_completion_batch_len",
        "miniss_max_io_completion_batch_items",
    ),
    (
        "miniss_inline_poll_total_ns",
        "miniss_inline_poll_duration_nanoseconds_total",
    ),
    (
        "miniss_max_inline_poll_ns",
        "miniss_inline_poll_duration_nanoseconds",
    ),
    (
        "miniss_wake_total_ns",
        "miniss_wake_duration_nanoseconds_total",
    ),
    ("miniss_max_wake_ns", "miniss_wake_duration_nanoseconds"),
    (
        "miniss_park_total_us",
        "miniss_park_duration_microseconds_total",
    ),
    ("miniss_max_park_us", "miniss_park_duration_microseconds"),
];

/// Every exported series descriptor.
pub fn series_descriptors() -> &'static [SeriesDescriptor] {
    SERIES
}

/// One completed aggregation pass over every shard of one runtime.
#[derive(Debug, Clone)]
pub struct MetricsView {
    /// Per-shard snapshots, ascending by shard id.
    pub runtime: RuntimeMetrics,
    /// When this pass completed.
    pub sampled_at: Instant,
    /// Wall time the pass cost, including the cross-core round trip.
    pub sample_duration: Duration,
    /// Sampling passes that failed consecutively before this one.  Zero for a
    /// fresh pass; non-zero means `runtime` is the last good view.
    pub consecutive_failures: u32,
}

/// Failure modes of the metrics scrape path.
///
/// Slot-backed scrapes read the per-shard publication slots directly on the
/// calling thread and cannot fail on a live runtime; the variants are retained
/// for API compatibility with the aggregator-thread design they described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetricsError {
    /// Historically: the periodic clock had not completed a pass yet.  Slot
    /// reads never produce this — a shard that has not published yet reports
    /// its true all-zero state.
    NotSampled,
    /// Historically: the aggregator thread was gone.  No aggregator thread
    /// exists any more; a quiesced runtime keeps answering from its frozen
    /// slots.
    AggregatorStopped,
    /// Historically: a sampling pass failed with no earlier view to fall back
    /// on.  Slot reads cannot fail mid-scrape.
    SampleFailed(String),
    /// Historically: the caller stopped waiting for the aggregator.  Slot
    /// reads never wait, so this is never produced.
    Timeout,
}

impl std::fmt::Display for MetricsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSampled => f.write_str("metrics have not been published yet"),
            Self::AggregatorStopped => f.write_str("metrics control plane is not running"),
            Self::SampleFailed(reason) => write!(f, "metrics sampling failed: {reason}"),
            Self::Timeout => f.write_str("timed out waiting for metrics"),
        }
    }
}

impl std::error::Error for MetricsError {}

/// Escape the two characters the text format reserves in `# HELP`.
fn escape_help(help: &str) -> std::borrow::Cow<'_, str> {
    if !help.contains('\\') && !help.contains('\n') {
        return std::borrow::Cow::Borrowed(help);
    }
    let mut escaped = String::with_capacity(help.len() + 8);
    for ch in help.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            other => escaped.push(other),
        }
    }
    std::borrow::Cow::Owned(escaped)
}

impl MetricsView {
    /// Render this view in the Prometheus text exposition format.
    ///
    /// Each metric emits `# HELP` and `# TYPE` once, then one
    /// `name{shard="N"} value` line per shard, then one unlabelled `name value`
    /// line holding the merged runtime-wide value.  Counters merge by
    /// summation, high-water marks by maximum; see [`Merge`].
    pub fn render_prometheus(&self) -> String {
        let shards = self.runtime.cores.len();
        let mut out = String::with_capacity(128 + shards * SERIES.len() * 40);
        for descriptor in SERIES {
            let _ = writeln!(
                out,
                "# HELP {} {}",
                descriptor.name,
                escape_help(descriptor.help)
            );
            let _ = writeln!(
                out,
                "# TYPE {} {}",
                descriptor.name,
                descriptor.metric_type.as_str()
            );
            for shard in &self.runtime.cores {
                let _ = writeln!(
                    out,
                    "{}{{shard=\"{}\"}} {}",
                    descriptor.name,
                    shard.core_id,
                    descriptor.read(shard)
                );
            }
            let _ = writeln!(
                out,
                "{} {}",
                descriptor.name,
                descriptor.merge_all(self.runtime.cores.iter())
            );
        }
        out
    }

    /// Read one value by metric name, for one shard or for the merged total.
    pub fn value(&self, name: &str, shard: Option<usize>) -> Option<u64> {
        let canonical_name = LEGACY_NAME_ALIASES
            .iter()
            .find_map(|(legacy, canonical)| (*legacy == name).then_some(*canonical))
            .unwrap_or(name);
        let descriptor = SERIES
            .iter()
            .find(|descriptor| descriptor.name == canonical_name)?;
        Some(match shard {
            Some(core_id) => descriptor.read(
                self.runtime
                    .cores
                    .iter()
                    .find(|snapshot| snapshot.core_id == core_id)?,
            ),
            None => descriptor.merge_all(self.runtime.cores.iter()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot() -> CoreMetricsSnapshot {
        CoreMetricsSnapshot {
            core_id: 0,
            local_task_count: 1,
            ready_queue_len: 2,
            max_ready_queue_len: 3,
            tasks_spawned: 4,
            tasks_completed: 5,
            tasks_polled: 6,
            io_completions: 7,
            messages_processed: 8,
            message_batches: 9,
            max_message_inbox_len: 10,
            busy_rounds: 11,
            flushes: 12,
            flush_total_us: 13,
            max_flush_us: 14,
            park_flushes: 15,
            park_flush_total_us: 16,
            max_park_flush_us: 17,
            io_completion_batches: 18,
            io_completion_total_ns: 19,
            max_io_completion_batch_ns: 20,
            max_io_completion_batch_len: 21,
            inline_poll_total_ns: 22,
            max_inline_poll_ns: 23,
            wake_total_ns: 24,
            max_wake_ns: 25,
            parks: 26,
            park_timeouts: 27,
            park_completions: 28,
            park_total_us: 29,
            max_park_us: 30,
            timer_expirations: 31,
            inline_completion_hits: 32,
            waker_wakeups: 33,
            quic_udp_tx_packets: 34,
            quic_udp_rx_packets: 35,
            quic_handshakes_accepted: 36,
            quic_handshakes_refused: 37,
            quic_connections_active: 38,
        }
    }

    /// Exhaustive destructuring: adding a field to `CoreMetricsSnapshot` without
    /// adding a matching `SERIES` entry breaks this test at compile time.
    #[test]
    fn snapshot_fields_and_series_stay_in_sync() {
        let snapshot = sample_snapshot();
        let CoreMetricsSnapshot {
            core_id: _,
            local_task_count,
            ready_queue_len,
            max_ready_queue_len,
            tasks_spawned,
            tasks_completed,
            tasks_polled,
            io_completions,
            messages_processed,
            message_batches,
            max_message_inbox_len,
            busy_rounds,
            flushes,
            flush_total_us,
            max_flush_us,
            park_flushes,
            park_flush_total_us,
            max_park_flush_us,
            io_completion_batches,
            io_completion_total_ns,
            max_io_completion_batch_ns,
            max_io_completion_batch_len,
            inline_poll_total_ns,
            max_inline_poll_ns,
            wake_total_ns,
            max_wake_ns,
            parks,
            park_timeouts,
            park_completions,
            park_total_us,
            max_park_us,
            timer_expirations,
            inline_completion_hits,
            waker_wakeups,
            quic_udp_tx_packets,
            quic_udp_rx_packets,
            quic_handshakes_accepted,
            quic_handshakes_refused,
            quic_connections_active,
        } = snapshot;
        let by_declaration_order = [
            local_task_count as u64,
            ready_queue_len as u64,
            max_ready_queue_len as u64,
            tasks_spawned,
            tasks_completed,
            tasks_polled,
            io_completions,
            messages_processed,
            message_batches,
            max_message_inbox_len as u64,
            busy_rounds,
            flushes,
            flush_total_us,
            max_flush_us,
            park_flushes,
            park_flush_total_us,
            max_park_flush_us,
            io_completion_batches,
            io_completion_total_ns,
            max_io_completion_batch_ns,
            max_io_completion_batch_len as u64,
            inline_poll_total_ns,
            max_inline_poll_ns,
            wake_total_ns,
            max_wake_ns,
            parks,
            park_timeouts,
            park_completions,
            park_total_us,
            max_park_us,
            timer_expirations,
            inline_completion_hits,
            waker_wakeups,
            quic_udp_tx_packets,
            quic_udp_rx_packets,
            quic_handshakes_accepted,
            quic_handshakes_refused,
            quic_connections_active as u64,
        ];

        assert_eq!(
            SERIES.len(),
            by_declaration_order.len(),
            "every CoreMetricsSnapshot field except core_id needs a SERIES entry"
        );
        for (descriptor, expected) in SERIES.iter().zip(by_declaration_order) {
            assert_eq!(
                descriptor.read(&snapshot),
                expected,
                "{} does not read the field it is paired with",
                descriptor.name
            );
        }
    }

    fn two_shard_view() -> MetricsView {
        let mut shard0 = sample_snapshot();
        shard0.core_id = 0;
        let mut shard1 = sample_snapshot();
        shard1.core_id = 1;
        shard1.tasks_spawned = 7;
        shard1.max_ready_queue_len = 99;
        shard1.local_task_count = 0;
        MetricsView {
            runtime: RuntimeMetrics {
                cores: vec![shard0, shard1],
            },
            sampled_at: Instant::now(),
            sample_duration: Duration::from_micros(250),
            consecutive_failures: 0,
        }
    }

    #[test]
    fn renders_help_type_shard_series_and_total() {
        let rendered = two_shard_view().render_prometheus();

        assert!(
            rendered.contains(
                "# HELP miniss_tasks_spawned_total Tasks admitted on the shard.\n\
                 # TYPE miniss_tasks_spawned_total counter\n\
                 miniss_tasks_spawned_total{shard=\"0\"} 4\n\
                 miniss_tasks_spawned_total{shard=\"1\"} 7\n\
                 miniss_tasks_spawned_total 11\n"
            ),
            "counter exposition is malformed:\n{rendered}"
        );

        // A maximum must not be summed.
        assert!(
            rendered.contains(
                "# TYPE miniss_max_ready_queue_tasks gauge\n\
                 miniss_max_ready_queue_tasks{shard=\"0\"} 3\n\
                 miniss_max_ready_queue_tasks{shard=\"1\"} 99\n\
                 miniss_max_ready_queue_tasks 99\n"
            ),
            "max series must merge by maximum:\n{rendered}"
        );

        // A gauge sums.
        assert!(
            rendered.contains(
                "# TYPE miniss_local_task_count gauge\n\
                 miniss_local_task_count{shard=\"0\"} 1\n\
                 miniss_local_task_count{shard=\"1\"} 0\n\
                 miniss_local_task_count 1\n"
            ),
            "gauge series must merge by summation:\n{rendered}"
        );
    }

    #[test]
    fn renamed_prometheus_series_keep_units_types_and_merge_semantics() {
        let renamed = [
            ("miniss_ready_queue_tasks", MetricType::Gauge, Merge::Sum),
            (
                "miniss_max_ready_queue_tasks",
                MetricType::Gauge,
                Merge::Max,
            ),
            (
                "miniss_max_message_inbox_messages",
                MetricType::Gauge,
                Merge::Max,
            ),
            (
                "miniss_flush_duration_microseconds_total",
                MetricType::Counter,
                Merge::Sum,
            ),
            (
                "miniss_flush_duration_microseconds",
                MetricType::Gauge,
                Merge::Max,
            ),
            (
                "miniss_park_flush_duration_microseconds_total",
                MetricType::Counter,
                Merge::Sum,
            ),
            (
                "miniss_park_flush_duration_microseconds",
                MetricType::Gauge,
                Merge::Max,
            ),
            (
                "miniss_io_completion_duration_nanoseconds_total",
                MetricType::Counter,
                Merge::Sum,
            ),
            (
                "miniss_io_completion_batch_duration_nanoseconds",
                MetricType::Gauge,
                Merge::Max,
            ),
            (
                "miniss_max_io_completion_batch_items",
                MetricType::Gauge,
                Merge::Max,
            ),
            (
                "miniss_inline_poll_duration_nanoseconds_total",
                MetricType::Counter,
                Merge::Sum,
            ),
            (
                "miniss_inline_poll_duration_nanoseconds",
                MetricType::Gauge,
                Merge::Max,
            ),
            (
                "miniss_wake_duration_nanoseconds_total",
                MetricType::Counter,
                Merge::Sum,
            ),
            (
                "miniss_wake_duration_nanoseconds",
                MetricType::Gauge,
                Merge::Max,
            ),
            (
                "miniss_park_duration_microseconds_total",
                MetricType::Counter,
                Merge::Sum,
            ),
            (
                "miniss_park_duration_microseconds",
                MetricType::Gauge,
                Merge::Max,
            ),
        ];
        for (name, metric_type, merge) in renamed {
            let descriptor = SERIES
                .iter()
                .find(|descriptor| descriptor.name == name)
                .unwrap_or_else(|| panic!("renamed metric {name} is missing"));
            assert_eq!(descriptor.metric_type, metric_type, "{name} type");
            assert_eq!(descriptor.merge, merge, "{name} merge");
        }

        let view = two_shard_view();
        let rendered = view.render_prometheus();
        for (legacy, canonical) in LEGACY_NAME_ALIASES {
            assert_eq!(
                view.value(legacy, None),
                view.value(canonical, None),
                "legacy lookup {legacy} must retain the canonical value"
            );
            assert!(
                !rendered.contains(legacy),
                "legacy name {legacy} must not be re-exported"
            );
        }

        for descriptor in SERIES {
            match descriptor.metric_type {
                MetricType::Counter => assert!(
                    descriptor.name.ends_with("_total"),
                    "counter {} must end in _total",
                    descriptor.name
                ),
                MetricType::Gauge => assert!(
                    !descriptor.name.ends_with("_total"),
                    "gauge {} must not end in _total",
                    descriptor.name
                ),
            }
        }
    }

    #[test]
    fn every_rendered_total_matches_the_shard_fold() {
        let view = two_shard_view();
        let rendered = view.render_prometheus();
        for descriptor in SERIES {
            let expected = descriptor.merge_all(view.runtime.cores.iter());
            assert_eq!(
                view.value(descriptor.name, None),
                Some(expected),
                "{} total lookup",
                descriptor.name
            );
            assert!(
                rendered.contains(&format!("\n{} {expected}\n", descriptor.name)),
                "{} has no matching unlabelled total line",
                descriptor.name
            );
            for shard in &view.runtime.cores {
                assert!(
                    rendered.contains(&format!(
                        "\n{}{{shard=\"{}\"}} {}\n",
                        descriptor.name,
                        shard.core_id,
                        descriptor.read(shard)
                    )),
                    "{} is missing the per-shard line for shard {}",
                    descriptor.name,
                    shard.core_id
                );
            }
        }
    }

    #[test]
    fn help_text_escapes_reserved_characters() {
        assert_eq!(escape_help("plain help"), "plain help");
        assert_eq!(escape_help("back\\slash"), "back\\\\slash");
        assert_eq!(escape_help("two\nlines"), "two\\nlines");
    }
}
