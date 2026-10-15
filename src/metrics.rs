//! Control-plane metrics aggregation and Prometheus text exposition.
//!
//! # Where the aggregator lives
//!
//! Every [`MultiCoreRuntime`](crate::multicore::MultiCoreRuntime) owns exactly
//! one aggregator thread.  It is started inside `MultiCoreRuntime::new`, in the
//! same pass that starts the workers, so nothing here is created after startup,
//! and `Drop for MultiCoreRuntime` stops and joins it.
//!
//! It is deliberately **not** a process-global registry:
//!
//! * AGENTS.md MUST NOT #4 forbids `static mut`, `lazy_static`,
//!   `once_cell::sync::`, and `std::sync::LazyLock`, so a global recorder is
//!   not merely discouraged, it is unimplementable here.
//! * The `metrics` + `metrics-exporter-prometheus` pair this replaces reaches
//!   for exactly that shape: a process-global recorder behind a one-time
//!   initialiser, with a global map of registered handles.  Both crates were
//!   removed from this repository for having zero call sites.
//! * A global would also be *wrong* here, not merely forbidden.  An aggregator
//!   can only see the shards of one runtime, so two runtimes in one process
//!   would merge into a single, meaningless series set.  Ownership by the
//!   runtime makes the series set well defined by construction.
//!
//! # Sampling
//!
//! The aggregator wakes on its own clock (default 1s, see
//! [`crate::config::metrics_interval`]) and fans one
//! [`crate::multicore::CoreMessage::CollectMetrics`] message per shard out over
//! the existing bounded inbox, through an internal fan-out handle.  A worker
//! answers by copying its shard-local counters: no atomics, no locks, and — the
//! point of the whole exercise — no per-request work.  AGENTS.md MUST #6 holds
//! by construction: the counters stay shard-local and are merged outside the hot
//! path.
//!
//! The aggregator reaches the shards through that fan-out handle, not through
//! the runtime itself.  Holding an `Arc<MultiCoreRuntime>` from a background
//! thread would keep the runtime alive forever and make `Arc::try_unwrap`
//! permanently fail for embedders; a `Weak` would leave that call racy whenever
//! a sampling pass happened to be in flight.
//!
//! Sampling is bounded: a full inbox, a disconnected shard, or a stalled shard
//! aborts the pass instead of blocking the sampler, and the previous good view
//! is retained and reported.  A pass also holds one admission ticket for its
//! whole fan-out, so `shutdown()` cannot close the gate while a pass is still
//! enqueueing — which is what keeps the post-join termination snapshot empty.
//!
//! # Shared-state inventory
//!
//! The aggregator thread owns its merged view as a plain local variable.  There
//! is no `Arc<Mutex<_>>`, no `RwLock`, no atomic, and no shared map.  Other
//! threads reach the view only by sending it a command and waiting for the reply
//! on a per-call `std::sync::mpsc` channel; every such wait is bounded by an
//! explicit deadline.

use std::fmt::Write as _;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender as ReplySender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::multicore::{CoreMessage, CoreMetricsSnapshot, CoreWakeup, RuntimeMetrics};
use crossbeam_channel::Sender;

/// Deadline for a sampling pass started by the periodic clock.
const DEFAULT_SAMPLE_TIMEOUT: Duration = Duration::from_millis(250);

/// The answer channel a [`MetricsCommand`] is answered on.
pub(crate) type MetricsReply = ReplySender<Result<MetricsView, MetricsError>>;

/// Command channel: aggregator replies, runtime and scrapers request.
pub(crate) type MetricsCommandSender = ReplySender<MetricsCommand>;

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
        "miniss_ready_queue_len",
        "Tasks currently runnable on the shard.",
        MetricType::Gauge,
        Merge::Sum,
        |s| s.ready_queue_len as u64,
    ),
    series(
        "miniss_max_ready_queue_len",
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
        "miniss_max_message_inbox_len",
        "High-water mark of the shard message inbox.",
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
        "miniss_flush_total_us",
        "Cumulative time spent flushing submissions.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.flush_total_us,
    ),
    series(
        "miniss_max_flush_us",
        "Slowest single submission flush.",
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
        "miniss_park_flush_total_us",
        "Cumulative flush time inside the park syscall.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.park_flush_total_us,
    ),
    series(
        "miniss_max_park_flush_us",
        "Slowest flush performed inside the park syscall.",
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
        "miniss_io_completion_total_ns",
        "Cumulative time spent draining IO completions.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.io_completion_total_ns,
    ),
    series(
        "miniss_max_io_completion_batch_ns",
        "Slowest IO completion batch.",
        MetricType::Gauge,
        Merge::Max,
        |s| s.max_io_completion_batch_ns,
    ),
    series(
        "miniss_max_io_completion_batch_len",
        "Largest IO completion batch.",
        MetricType::Gauge,
        Merge::Max,
        |s| s.max_io_completion_batch_len as u64,
    ),
    series(
        "miniss_inline_poll_total_ns",
        "Cumulative time spent polling inline completions.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.inline_poll_total_ns,
    ),
    series(
        "miniss_max_inline_poll_ns",
        "Slowest inline completion poll.",
        MetricType::Gauge,
        Merge::Max,
        |s| s.max_inline_poll_ns,
    ),
    series(
        "miniss_wake_total_ns",
        "Cumulative time spent waking tasks.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.wake_total_ns,
    ),
    series(
        "miniss_max_wake_ns",
        "Slowest single wake.",
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
        "miniss_park_total_us",
        "Cumulative time parked.",
        MetricType::Counter,
        Merge::Sum,
        |s| s.park_total_us,
    ),
    series(
        "miniss_max_park_us",
        "Slowest single park.",
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

/// Failure modes of the control-plane aggregator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetricsError {
    /// The periodic clock has not completed a pass yet.
    NotSampled,
    /// The aggregator thread is gone; the runtime is shutting down or dropped.
    AggregatorStopped,
    /// A pass failed and no earlier pass produced a view to fall back on.
    SampleFailed(String),
    /// The caller stopped waiting for the aggregator.
    Timeout,
}

impl std::fmt::Display for MetricsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSampled => f.write_str("metrics aggregator has not completed a pass yet"),
            Self::AggregatorStopped => f.write_str("metrics aggregator thread is not running"),
            Self::SampleFailed(reason) => write!(f, "metrics sampling failed: {reason}"),
            Self::Timeout => f.write_str("timed out waiting for the metrics aggregator"),
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
        let descriptor = SERIES.iter().find(|descriptor| descriptor.name == name)?;
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

/// Control-plane sampling transport handed to the aggregator.
///
/// This is what the aggregator thread holds instead of an
/// `Arc<MultiCoreRuntime>`.  A background thread that kept the runtime alive
/// would make `Arc::try_unwrap(runtime)` permanently fail for embedders, and a
/// `Weak` would leave that call racy whenever a pass happened to be in flight.
/// Holding only the fan-out keeps the runtime's ownership graph exactly as it
/// was: the caller still decides when the runtime dies.
///
/// Everything in here is read-only after startup.  The per-shard
/// `Sender`s are clones of the same bounded inboxes the spawn path uses, and
/// the [`CoreWakeup`] handles are `Arc`s the workers do not own, so no lock,
/// atomic, or `Rc` crosses the thread boundary here.
pub(crate) struct MetricsFanout {
    pub(crate) senders: Vec<Sender<CoreMessage>>,
    pub(crate) threads: Vec<std::thread::Thread>,
    pub(crate) wakeups: Vec<Arc<CoreWakeup>>,
    /// Same admission counter the spawn paths use.  A sampling pass holds one
    /// ticket for its whole fan-out, so `close_admission_and_wait` cannot
    /// return while a pass is still enqueueing.
    pub(crate) admission: Arc<AtomicUsize>,
}

impl MetricsFanout {
    /// Fan one `CollectMetrics` message out to every shard and collect the
    /// answers.
    ///
    /// This is the same bounded control-plane path `spawn` uses: one admission
    /// ticket, a `try_send` per shard (so a saturated inbox fails the pass
    /// instead of blocking), a wake per shard, and an overall deadline.  A
    /// worker answers by copying its shard-local counters, so the worker pays
    /// nothing per request.
    ///
    /// Holding an admission ticket is what makes shutdown safe: `shutdown()`
    /// closes the gate and waits for outstanding tickets, so a pass can never
    /// enqueue into a shard that has already passed its drain check.
    pub(crate) fn sample(&self, timeout: Duration) -> std::result::Result<RuntimeMetrics, String> {
        let _admission =
            crate::multicore::begin_admission_on(&self.admission).map_err(|err| err.to_string())?;
        let (reply, receiver) = std::sync::mpsc::channel();
        for (core_id, sender) in self.senders.iter().enumerate() {
            sender
                .try_send(CoreMessage::CollectMetrics {
                    reply: reply.clone(),
                })
                .map_err(|err| format!("Core {core_id} metrics request failed: {err}"))?;
            self.wake(core_id);
        }
        drop(reply);

        let deadline = Instant::now() + timeout;
        let mut cores = Vec::with_capacity(self.senders.len());
        while cores.len() < self.senders.len() {
            let now = Instant::now();
            if now >= deadline {
                return Err(format!(
                    "Timed out collecting metrics: got {}/{} cores",
                    cores.len(),
                    self.senders.len()
                ));
            }
            match receiver.recv_timeout(deadline.saturating_duration_since(now)) {
                Ok(snapshot) => cores.push(snapshot),
                Err(err) => return Err(format!("Metrics collection failed: {err}")),
            }
        }
        cores.sort_by_key(|snapshot| snapshot.core_id);
        Ok(RuntimeMetrics { cores })
    }

    fn wake(&self, core_id: usize) {
        self.threads[core_id].unpark();
        self.wakeups[core_id].notify();
    }
}

/// Requests the aggregator thread serves.  This enum is the *only* shared
/// state between the aggregator and its callers.
pub(crate) enum MetricsCommand {
    /// Answer with the most recently merged view, without forcing a pass.
    View(MetricsReply),
    /// Run one bounded pass now and answer with the fresh view.
    Refresh {
        timeout: Duration,
        reply: MetricsReply,
    },
    /// Leave the loop; sent by `Drop for MultiCoreRuntime`.
    Stop,
}

/// Owns the merged view.  Never shared: it is a local of the aggregator thread
/// for the whole life of the thread.
pub(crate) struct Aggregator {
    interval: Duration,
    latest: Option<MetricsView>,
    consecutive_failures: u32,
}

impl Aggregator {
    pub(crate) fn new(interval: Duration) -> Self {
        Self {
            interval,
            latest: None,
            consecutive_failures: 0,
        }
    }

    fn latest_view(&self) -> Result<MetricsView, MetricsError> {
        self.latest.clone().ok_or(MetricsError::NotSampled)
    }

    /// Run one bounded aggregation pass and adopt the result.
    ///
    /// A failed pass keeps the previous view and records the failure, so a
    /// saturated inbox degrades into a stale sample instead of an outage.
    fn sample(
        &mut self,
        fanout: &MetricsFanout,
        timeout: Duration,
    ) -> Result<MetricsView, MetricsError> {
        let started = Instant::now();
        match fanout.sample(timeout) {
            Ok(sharded) => {
                self.consecutive_failures = 0;
                let view = MetricsView {
                    runtime: sharded,
                    sampled_at: Instant::now(),
                    sample_duration: started.elapsed(),
                    consecutive_failures: 0,
                };
                self.latest = Some(view.clone());
                Ok(view)
            }
            Err(err) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                tracing::debug!(
                    failures = self.consecutive_failures,
                    "metrics sampling pass failed: {err}"
                );
                match self.latest.clone() {
                    Some(view) => Ok(view),
                    None => Err(MetricsError::SampleFailed(err)),
                }
            }
        }
    }

    /// Periodic sampling loop.  Runs until `Stop` or until the command channel
    /// is dropped, which is why `Drop for MultiCoreRuntime` joins it.
    pub(crate) fn run(mut self, fanout: Arc<MetricsFanout>, commands: Receiver<MetricsCommand>) {
        // The deadline is carried across iterations on purpose.  Recomputing it
        // from `recv_timeout` per iteration would let a scrape stream faster
        // than the interval hold the timer off forever: every command resets
        // the wait and the periodic pass never becomes due.
        let mut next_pass = Instant::now() + self.interval;
        loop {
            let wait = next_pass.saturating_duration_since(Instant::now());
            match commands.recv_timeout(wait) {
                // Answering is free, so a scrape must not delay the clock.
                Ok(MetricsCommand::View(reply)) => {
                    let _ = reply.send(self.latest_view());
                }
                Ok(MetricsCommand::Refresh { timeout, reply }) => {
                    let result = self.sample(&fanout, timeout);
                    let _ = reply.send(result);
                    next_pass = Instant::now() + self.interval;
                }
                Ok(MetricsCommand::Stop) => break,
                // Periodic pass.  Bounded by `DEFAULT_SAMPLE_TIMEOUT`; it never
                // blocks the data plane because each shard answers from its own
                // shard-local counters.
                Err(RecvTimeoutError::Timeout) => {
                    let _ = self.sample(&fanout, DEFAULT_SAMPLE_TIMEOUT);
                    next_pass = Instant::now() + self.interval;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }
}

/// Control-plane handles to the aggregator, owned by one `MultiCoreRuntime`.
pub(crate) struct MetricsControl {
    /// Commands.  MPSC, so many scrape threads can share one aggregator.
    pub(crate) commands: MetricsCommandSender,
    /// Sampling transport, shared with `MultiCoreRuntime::metrics_snapshot`.
    pub(crate) fanout: Arc<MetricsFanout>,
    /// Aggregator thread; stopped and joined on runtime drop.
    pub(crate) sampler: Option<std::thread::JoinHandle<()>>,
    /// Periodic pass interval chosen at startup.
    pub(crate) interval: Duration,
}

impl MetricsControl {
    /// Ask the aggregator a question and wait a bounded time for the answer.
    ///
    /// This is the only cross-thread path into the merged view.  There is no
    /// lock to take and no shared cell to tear: the aggregator thread holds the
    /// view as a plain local and answers through this per-call channel.
    pub(crate) fn ask(
        &self,
        timeout: Duration,
        build: impl FnOnce(MetricsReply) -> MetricsCommand,
    ) -> Result<MetricsView, MetricsError> {
        let (reply, answers) = std::sync::mpsc::channel();
        self.commands
            .send(build(reply))
            .map_err(|_| MetricsError::AggregatorStopped)?;
        answers
            .recv_timeout(timeout)
            .map_err(|_| MetricsError::Timeout)?
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
                "# TYPE miniss_max_ready_queue_len gauge\n\
                 miniss_max_ready_queue_len{shard=\"0\"} 3\n\
                 miniss_max_ready_queue_len{shard=\"1\"} 99\n\
                 miniss_max_ready_queue_len 99\n"
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
