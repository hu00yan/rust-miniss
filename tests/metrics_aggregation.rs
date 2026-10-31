//! Control-plane metrics publication and Prometheus export.
//!
//! These tests assert *values*, not presence: the merged runtime-wide series
//! must be the arithmetic fold of the per-shard series, and the per-shard
//! values must be the ones the traffic actually produced. Workers publish
//! their counters into single-writer slots at the event-loop boundary —
//! rate-limited by `MINISS_METRICS_PUBLISH_MIN_MS` (default 10 s) — and
//! scrapes read those slots directly, so every assertion here also exercises
//! that publication path. Published snapshots carry cumulative counters since
//! process start, so a rate-limited scrape may observe data up to one
//! interval old without ever being wrong; tests that need turn-fresh values
//! set the interval to `0`, and the final-publication test proves the exit
//! path ignores the interval entirely.

use rust_miniss::metrics::{series_descriptors, Merge, MetricType};
use rust_miniss::MultiCoreRuntime;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
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

/// Guard that sets `MINISS_METRICS_PUBLISH_MIN_MS` for one test and removes it
/// again on drop (the default 10 s interval then applies to later tests).
///
/// Every runtime-creating test in this binary holds `RUNTIME_TEST_LOCK` for
/// its whole body, so no sibling creates a worker that would read the variable
/// concurrently; test binaries are separate processes, so the lib tests never
/// see it.
struct PublishMinGuard;

fn set_publish_min_ms(ms: &str) -> PublishMinGuard {
    // SAFETY: tests serialize on RUNTIME_TEST_LOCK; set_var is safe here
    // because no other thread reads this variable concurrently.
    unsafe {
        std::env::set_var("MINISS_METRICS_PUBLISH_MIN_MS", ms);
    }
    PublishMinGuard
}

impl Drop for PublishMinGuard {
    fn drop(&mut self) {
        // SAFETY: as `set_publish_min_ms`.
        unsafe {
            std::env::remove_var("MINISS_METRICS_PUBLISH_MIN_MS");
        }
    }
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
    // Exact per-shard values are asserted below, so publication must be
    // turn-fresh here: `0` publishes on every loop turn.
    let _publish_min = set_publish_min_ms("0");

    const SHARDS: usize = 2;
    const TASKS: usize = 9;
    // Explicit shard placement keeps the expected per-shard split exact instead
    // of depending on round-robin racing the publication of other traffic.
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
    assert_eq!(view.consecutive_failures, 0, "a slot read cannot fail");

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
fn publication_is_visible_without_any_sampler_pass() {
    let _test_lock = runtime_test_lock();
    init_tracing();
    // This test asserts first-scrape visibility and growing `parks`, both of
    // which require turn-fresh publication: `0` publishes on every loop turn.
    // (Under the default 10 s rate limit the same assertions hold against the
    // first publication's data or after the final one — cumulative counters
    // make the stale view correct, just older.)
    let _publish_min = set_publish_min_ms("0");

    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
    let interval = runtime.metrics_interval();
    assert_eq!(
        interval,
        rust_miniss::config::metrics_interval(),
        "the runtime must use the startup-resolved metrics cadence"
    );

    // No control-plane thread and no periodic pass exists: the first scrape
    // after traffic must already reflect what the workers published at their
    // own event-loop boundaries.  Nothing here waits on a clock before the
    // first scrape.
    const TASKS: usize = 4;
    const TASKS_PER_SHARD: usize = TASKS / 2;
    let (tx, rx) = mpsc::channel();
    for core_id in 0..2 {
        for _ in 0..TASKS_PER_SHARD {
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

    let first = runtime.metrics_view().unwrap();
    assert_eq!(first.consecutive_failures, 0);
    assert_eq!(first.runtime.cores.len(), 2);
    let spawned: u64 = first
        .runtime
        .cores
        .iter()
        .map(|shard| shard.tasks_spawned)
        .sum();
    assert!(
        spawned >= TASKS as u64,
        "loop-boundary publication must be visible on the first scrape, got {spawned}"
    );

    // Park accounting is published by idle workers on their own loop boundary.
    // No scrape triggers it and no sampler clock exists, so growing `parks`
    // here is direct evidence of worker-side push without any reader.
    let deadline = Instant::now() + Duration::from_secs(5);
    let latest = loop {
        assert!(
            Instant::now() < deadline,
            "idle workers never published park accounting"
        );
        std::thread::sleep(Duration::from_millis(20));
        let view = runtime.metrics_view().unwrap();
        if view.runtime.cores.iter().all(|shard| shard.parks > 0) {
            break view;
        }
    };

    // Counters are monotonic across scrapes.
    for (before, after) in first.runtime.cores.iter().zip(&latest.runtime.cores) {
        assert!(after.tasks_polled >= before.tasks_polled);
        assert!(after.parks >= before.parks);
    }

    runtime
        .shutdown_with_timeout(Duration::from_secs(5))
        .unwrap();
    runtime.join().unwrap();
}

/// The final publication must ignore the minimum publication interval: with
/// the loop-boundary publish rate-limited far beyond this test's duration,
/// any post-shutdown freshness can only come from the exit path's
/// unconditional publication (`CpuCore::publish_metrics`).
#[test]
fn final_publication_fires_regardless_of_the_publication_interval() {
    let _test_lock = runtime_test_lock();
    init_tracing();
    // One hour: no loop-boundary publish can fire during this test after the
    // worker's first turn.
    let _publish_min = set_publish_min_ms("3600000");

    const TASKS: u64 = 24;
    let runtime = MultiCoreRuntime::new(Some(1)).unwrap();
    let (tx, rx) = mpsc::channel();
    for _ in 0..TASKS {
        let tx = tx.clone();
        runtime
            .spawn(async move {
                tx.send(()).unwrap();
            })
            .unwrap();
    }
    drop(tx);
    for _ in 0..TASKS {
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    // Let the idle worker run park cycles: its park accounting accumulates
    // locally but stays unpublished while the rate limit holds.
    std::thread::sleep(Duration::from_millis(20));

    // Mid-run the slot may legitimately be stale — cumulative counters mean
    // the scrape lags behind the traffic, never lies about it.
    let stale = runtime.metrics_view().unwrap();
    assert!(stale.runtime.cores[0].tasks_spawned <= TASKS);

    runtime
        .shutdown_with_timeout(Duration::from_secs(5))
        .unwrap();
    runtime.join().unwrap();

    // The final publication ignored the interval: the frozen slot holds the
    // counters of the worker's last turn exactly.
    let final_view = runtime.metrics_view().unwrap();
    assert_eq!(
        final_view.value("miniss_tasks_spawned_total", None),
        Some(TASKS),
        "the final publication must carry the last turn's spawned count"
    );
    assert_eq!(
        final_view.value("miniss_tasks_completed_total", None),
        Some(TASKS),
        "the final publication must carry the last turn's completed count"
    );
    assert!(
        final_view.runtime.cores[0].parks > 0,
        "the final publication must carry post-park accounting despite the rate limit"
    );
}

#[test]
fn scrapes_during_shutdown_leave_no_control_state_behind() {
    let _test_lock = runtime_test_lock();
    init_tracing();

    // Repeated shutdowns while a scrape loop is running: a slot read never
    // enqueues anything into a worker inbox, so a scrape concurrent with
    // `shutdown()` must leave no control message — and no other control state
    // — behind in the post-join termination snapshot.  This is the regression
    // guard for any future change that reintroduces control-plane traffic into
    // the scrape path.
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
        // Keep scraping right up to the shutdown cutoff so any hidden
        // control-plane round trip would be as likely as possible to be in
        // flight when `shutdown()` closes admission.
        let stop = Arc::new(AtomicBool::new(false));
        let scraper = {
            let runtime = Arc::clone(&runtime);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let _ = runtime.metrics_refresh(Duration::from_millis(200));
                }
            })
        };
        std::thread::sleep(Duration::from_millis(60));
        runtime
            .shutdown_with_timeout(Duration::from_secs(5))
            .unwrap();
        stop.store(true, Ordering::Release);
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
fn a_quiesced_runtime_keeps_answering_from_frozen_slots() {
    let _test_lock = runtime_test_lock();
    init_tracing();

    let runtime = MultiCoreRuntime::new(Some(1)).unwrap();
    let baseline = runtime.metrics_refresh(Duration::from_secs(2)).unwrap();
    assert_eq!(baseline.consecutive_failures, 0);

    runtime
        .shutdown_with_timeout(Duration::from_secs(5))
        .unwrap();
    runtime.join().unwrap();

    // The workers are gone and their slots are frozen at the final
    // publication, but the runtime object still owns the slots: scrapes must
    // keep answering, bounded, with identical counter values.
    let scrape_started = Instant::now();
    let frozen = runtime
        .metrics_refresh(Duration::from_millis(500))
        .unwrap_or_else(|err| panic!("frozen slots must stay readable: {err}"));
    assert!(
        scrape_started.elapsed() < Duration::from_secs(10),
        "a post-join scrape must stay bounded"
    );
    assert_eq!(frozen.consecutive_failures, 0);
    assert_eq!(frozen.runtime.cores.len(), 1);

    let frozen_again = runtime.metrics_view().unwrap();
    for descriptor in series_descriptors() {
        assert_eq!(
            frozen.value(descriptor.name, Some(0)),
            frozen_again.value(descriptor.name, Some(0)),
            "{} must be frozen after join",
            descriptor.name
        );
    }

    // Between the baseline and the freeze the workers may still have worked,
    // so counters only move forward.
    for descriptor in series_descriptors() {
        if descriptor.metric_type != MetricType::Counter {
            continue;
        }
        let before = baseline.value(descriptor.name, Some(0));
        let after = frozen.value(descriptor.name, Some(0));
        assert!(
            after >= before,
            "{} must not go backwards across shutdown",
            descriptor.name
        );
    }

    drop(runtime);
}

#[test]
fn concurrent_scrapes_during_worker_activity_stay_consistent() {
    let _test_lock = runtime_test_lock();
    init_tracing();
    // The scraper hammers the slots while workers publish at their boundary;
    // `0` keeps the publication rate high so the seqlock retry path is
    // actually stressed, and the post-quiesce convergence scrape below sees
    // the traffic's terminal state within a turn.
    let _publish_min = set_publish_min_ms("0");

    const SHARDS: usize = 2;
    const TASKS: usize = 400;
    const TASKS_PER_SHARD: usize = TASKS / SHARDS;

    let runtime = Arc::new(MultiCoreRuntime::new(Some(SHARDS)).unwrap());

    // One scraper hammering `metrics_view` while workers publish at their loop
    // boundary: this is the publication retry-path stress.  Every scrape must
    // be internally consistent (merged total == fold of the per-shard series
    // in the same view), per-shard counters must never go backwards, and no
    // shard may report more completed than spawned at a turn boundary.
    let stop = Arc::new(AtomicBool::new(false));
    let scraper = {
        let runtime = Arc::clone(&runtime);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut last: BTreeMap<usize, BTreeMap<&'static str, u64>> = BTreeMap::new();
            let mut scrapes = 0usize;
            while !stop.load(Ordering::Acquire) {
                let view = runtime.metrics_view().unwrap_or_else(|err| {
                    panic!("scrape {scrapes} must succeed under load: {err}")
                });
                for descriptor in series_descriptors() {
                    let shard_values: Vec<(usize, u64)> = view
                        .runtime
                        .cores
                        .iter()
                        .map(|shard| (shard.core_id, descriptor.read(shard)))
                        .collect();
                    let folded = match descriptor.merge {
                        Merge::Sum => shard_values
                            .iter()
                            .map(|(_, value)| *value)
                            .fold(0u64, u64::saturating_add),
                        Merge::Max => shard_values
                            .iter()
                            .map(|(_, value)| *value)
                            .max()
                            .unwrap_or(0),
                    };
                    assert_eq!(
                        view.value(descriptor.name, None),
                        Some(folded),
                        "{} total must equal the shard fold during concurrent publication",
                        descriptor.name
                    );
                    if descriptor.metric_type == MetricType::Counter {
                        for (core_id, value) in shard_values {
                            let previous = last
                                .entry(core_id)
                                .or_default()
                                .insert(descriptor.name, value);
                            if let Some(previous) = previous {
                                assert!(
                                    value >= previous,
                                    "{} on shard {core_id} went backwards: \
                                     {previous} -> {value}",
                                    descriptor.name
                                );
                            }
                        }
                    }
                }
                for shard in &view.runtime.cores {
                    assert!(
                        shard.tasks_spawned >= shard.tasks_completed,
                        "shard {} reported completed > spawned at a turn boundary",
                        shard.core_id
                    );
                }
                scrapes += 1;
            }
            scrapes
        })
    };

    let (tx, rx) = mpsc::channel();
    for index in 0..TASKS {
        let core_id = index % SHARDS;
        let tx = tx.clone();
        runtime
            .spawn_on(core_id, async move {
                tx.send(()).unwrap();
            })
            .unwrap();
    }
    drop(tx);
    for _ in 0..TASKS {
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    stop.store(true, Ordering::Release);
    let scrapes = scraper.join().unwrap();
    assert!(
        scrapes > 0,
        "the scraper must have completed at least one scrape"
    );

    // Convergence: once traffic quiesces and publication settles, one more
    // scrape must show exactly the traffic's terminal state, per shard.
    let view = runtime.metrics_refresh(Duration::from_secs(2)).unwrap();
    assert_eq!(
        view.value("miniss_tasks_spawned_total", None),
        Some(TASKS as u64)
    );
    assert_eq!(
        view.value("miniss_tasks_completed_total", None),
        Some(TASKS as u64)
    );
    assert_eq!(
        view.value("miniss_tasks_spawned_total", Some(0)),
        Some(TASKS_PER_SHARD as u64)
    );
    assert_eq!(
        view.value("miniss_tasks_spawned_total", Some(1)),
        Some(TASKS_PER_SHARD as u64)
    );

    runtime
        .shutdown_with_timeout(Duration::from_secs(5))
        .unwrap();
    runtime.join().unwrap();
}
