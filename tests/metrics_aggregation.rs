//! Control-plane metrics aggregation and Prometheus export.
//!
//! These tests assert *values*, not presence: the merged runtime-wide series
//! must be the arithmetic fold of the per-shard series, and the per-shard
//! values must be the ones the traffic actually produced.

use rust_miniss::metrics::{series_descriptors, Merge, MetricsError};
use rust_miniss::MultiCoreRuntime;
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A runtime owns worker threads and pins them to shard CPUs.  Serialize this
/// binary's runtime tests so concurrent startup/shutdown cannot contend on the
/// same affinity plan or leave workers behind.  This is a test-serialisation
/// helper, not runtime state: it is a `parking_lot` lock rather than a
/// `std::sync` one only because the architecture audit forbids that spelling
/// outright, in tests as much as in `src`.
static RUNTIME_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

fn runtime_test_lock() -> parking_lot::MutexGuard<'static, ()> {
    RUNTIME_TEST_LOCK.lock()
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

/// Parse the exposition text into `metric -> {shard label -> value}` plus the
/// unlabelled total under the `None` key.
fn parse_exposition(text: &str) -> BTreeMap<&str, BTreeMap<Option<usize>, u64>> {
    let mut parsed: BTreeMap<&str, BTreeMap<Option<usize>, u64>> = BTreeMap::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let (series, value) = line
            .rsplit_once(' ')
            .unwrap_or_else(|| panic!("unterminated sample line: {line}"));
        let value: u64 = value
            .parse()
            .unwrap_or_else(|_| panic!("non-integer sample value on line: {line}"));
        let (name, shard) = match series.split_once('{') {
            Some((name, labels)) => {
                let shard = labels
                    .strip_suffix('}')
                    .and_then(|labels| labels.strip_prefix("shard=\""))
                    .and_then(|shard| shard.strip_suffix('"'))
                    .and_then(|shard| shard.parse::<usize>().ok())
                    .unwrap_or_else(|| panic!("unsupported label set: {line}"));
                (name, Some(shard))
            }
            None => (series, None),
        };
        assert_eq!(
            parsed.entry(name).or_default().insert(shard, value),
            None,
            "duplicate series {name}"
        );
    }
    parsed
}

#[test]
fn aggregation_is_the_arithmetic_fold_of_every_shard() {
    let _test_lock = runtime_test_lock();
    init_tracing();

    const SHARDS: usize = 2;
    const TASKS: usize = 9;
    // Explicit shard placement keeps the expected per-shard split exact instead
    // of depending on round-robin racing the sampler thread's own messages.
    const TASKS_ON_SHARD_0: usize = 5;

    let runtime = MultiCoreRuntime::new(Some(SHARDS)).unwrap();
    let (tx, rx) = mpsc::channel();

    for core_id in 0..SHARDS {
        let on_this_shard = if core_id == 0 {
            TASKS_ON_SHARD_0
        } else {
            TASKS - TASKS_ON_SHARD_0
        };
        for _ in 0..on_this_shard {
            let tx = tx.clone();
            runtime
                .spawn_on(core_id, async move {
                    tx.send(()).unwrap();
                })
                .unwrap();
        }
    }
    drop(tx);
    for _ in 0..TASKS {
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    let view = runtime.metrics_refresh(Duration::from_secs(2)).unwrap();
    assert_eq!(view.runtime.cores.len(), SHARDS);
    assert_eq!(view.runtime.cores[0].core_id, 0);
    assert_eq!(view.runtime.cores[1].core_id, 1);
    assert_eq!(view.consecutive_failures, 0, "the pass must be fresh");

    let rendered = view.render_prometheus();
    let parsed = parse_exposition(&rendered);

    // Per-shard values are the ones the traffic produced.
    let spawned = &parsed["miniss_tasks_spawned_total"];
    assert_eq!(spawned[&Some(0)], TASKS_ON_SHARD_0 as u64);
    assert_eq!(spawned[&Some(1)], (TASKS - TASKS_ON_SHARD_0) as u64);
    assert_eq!(spawned[&Some(0)] + spawned[&Some(1)], TASKS as u64);
    // Every task sent its ack, so every task also reached a terminal state.
    assert_eq!(parsed["miniss_tasks_completed_total"][&None], TASKS as u64);
    // Polling a task to completion takes at least one poll per task.
    assert!(parsed["miniss_tasks_polled_total"][&None] >= TASKS as u64);

    // And every exported series merges exactly as its descriptor promises.
    for descriptor in series_descriptors() {
        let exported = parsed
            .get(descriptor.name)
            .unwrap_or_else(|| panic!("{} is missing from the exposition", descriptor.name));
        let shard_values: BTreeMap<usize, u64> = exported
            .iter()
            .filter_map(|(shard, value)| shard.map(|id| (id, *value)))
            .collect();
        assert_eq!(
            shard_values.keys().copied().collect::<Vec<_>>(),
            vec![0, 1],
            "{} must carry one series per shard",
            descriptor.name
        );
        let folded = match descriptor.merge {
            Merge::Sum => shard_values.values().copied().sum(),
            Merge::Max => shard_values.values().copied().max().unwrap(),
        };
        assert_eq!(
            exported[&None], folded,
            "{} total does not match the shard fold",
            descriptor.name
        );
        // Cross-check against the structured view, not only the text.
        for (core_id, value) in &shard_values {
            assert_eq!(view.value(descriptor.name, Some(*core_id)), Some(*value));
        }
        assert_eq!(view.value(descriptor.name, None), Some(folded));
    }

    runtime
        .shutdown_with_timeout(Duration::from_secs(5))
        .unwrap();
    runtime.join().unwrap();
}

#[test]
fn the_aggregator_runs_on_its_own_clock() {
    let _test_lock = runtime_test_lock();
    init_tracing();

    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
    let interval = runtime.metrics_interval();
    assert_eq!(
        interval,
        rust_miniss::config::metrics_interval(),
        "the runtime must use the startup-resolved aggregation interval"
    );

    // The aggregator samples on a timer, not on request traffic.  No task is
    // spawned and no scrape is in flight here, so the view can only change if
    // the periodic clock fired on its own.
    let deadline = Instant::now() + 3 * interval + Duration::from_secs(5);
    let mut previous = loop {
        match runtime.metrics_view() {
            Ok(view) => break view,
            Err(MetricsError::NotSampled) => {
                assert!(
                    Instant::now() < deadline,
                    "aggregator never completed a periodic pass within {:?}",
                    3 * interval + Duration::from_secs(5)
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("aggregator must be answering: {err}"),
        }
    };
    let first_sampled_at = previous.sampled_at;

    // A second periodic pass must arrive on the clock, with no scrape and no
    // task traffic in between.
    loop {
        assert!(
            Instant::now() < deadline,
            "aggregator did not run a second periodic pass within {:?}",
            3 * interval + Duration::from_secs(5)
        );
        std::thread::sleep(Duration::from_millis(20));
        previous = runtime
            .metrics_view()
            .unwrap_or_else(|err| panic!("aggregator must be answering: {err}"));
        if previous.sampled_at > first_sampled_at {
            break;
        }
    }
    assert!(previous.runtime.cores.iter().all(|shard| shard.parks > 0));

    let later = runtime.metrics_refresh(Duration::from_secs(2)).unwrap();
    assert!(
        later.sampled_at > previous.sampled_at,
        "a forced pass must be newer than the periodic one"
    );
    // Counters are monotonic, so a later pass never reports less work.
    for (before, after) in previous.runtime.cores.iter().zip(&later.runtime.cores) {
        assert!(after.tasks_polled >= before.tasks_polled);
        assert!(after.parks >= before.parks);
    }

    runtime
        .shutdown_with_timeout(Duration::from_secs(5))
        .unwrap();
    runtime.join().unwrap();
}

#[test]
fn sampling_never_leaves_a_control_message_in_a_terminating_worker() {
    let _test_lock = runtime_test_lock();
    init_tracing();

    // Repeated shutdowns while the periodic clock is firing: a sampling pass
    // must never enqueue `CollectMetrics` into a shard that has already passed
    // its drain check, which would show up as a non-empty inbox in the
    // post-join termination snapshot.
    for round in 0..8 {
        let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
        let (tx, rx) = mpsc::channel();
        for core_id in 0..2 {
            let tx = tx.clone();
            runtime
                .spawn_on(core_id, async move {
                    tx.send(()).ok();
                })
                .unwrap();
        }
        drop(tx);
        for _ in 0..2 {
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        // Keep scraping right up to the shutdown cutoff so a pass is as likely
        // as possible to be in flight when `shutdown()` closes admission.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let scraper = {
            let runtime = std::sync::Arc::clone(&runtime);
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    let _ = runtime.metrics_refresh(Duration::from_millis(200));
                }
            })
        };
        std::thread::sleep(Duration::from_millis(60));
        runtime
            .shutdown_with_timeout(Duration::from_secs(5))
            .unwrap();
        stop.store(true, std::sync::atomic::Ordering::Release);
        scraper.join().unwrap();
        runtime.join().unwrap();

        let snapshot = runtime.termination_snapshot().unwrap();
        assert!(
            snapshot.all_zero(),
            "round {round} left control state behind: {snapshot:?}"
        );
        drop(runtime);
    }
}

#[test]
fn a_stopped_aggregator_reports_instead_of_hanging() {
    let _test_lock = runtime_test_lock();
    init_tracing();

    let runtime = MultiCoreRuntime::new(Some(1)).unwrap();
    let successful = runtime.metrics_refresh(Duration::from_secs(2)).unwrap();
    assert_eq!(successful.consecutive_failures, 0);

    let started = Instant::now();
    runtime
        .shutdown_with_timeout(Duration::from_secs(5))
        .unwrap();
    runtime.join().unwrap();

    // A pass against terminated shards cannot succeed. The aggregator must
    // return the last good view with an updated failure count, not block or
    // return a stale view that still looks fresh.
    let stale = runtime
        .metrics_refresh(Duration::from_millis(500))
        .unwrap_or_else(|err| panic!("must retain the prior good view: {err}"));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a post-join refresh must stay bounded"
    );
    assert_eq!(stale.sampled_at, successful.sampled_at);
    assert_eq!(stale.runtime.cores.len(), 1);
    assert!(
        stale.consecutive_failures > successful.consecutive_failures,
        "the returned last-good view must reflect the failed sample"
    );

    // The read-only path keeps answering with the same updated failure count.
    let retained = runtime
        .metrics_view()
        .unwrap_or_else(|err| panic!("last good view should remain readable: {err}"));
    assert_eq!(retained.sampled_at, successful.sampled_at);
    assert_eq!(retained.consecutive_failures, stale.consecutive_failures);

    drop(runtime);
}
