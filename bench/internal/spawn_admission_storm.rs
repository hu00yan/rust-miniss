//! Spawn-admission storm baseline — the T3 spawn phase (T3
//! `quic_conn_lifecycle.rs`, feat/quic-service-lifecycle) ported to a
//! backend-independent bench so it can run on main without the QUIC-only
//! acceptor API. Measured quantity unchanged: 20,000 `spawn_local` spawns of
//! an immediately-completing task on one shard, timed from spawn start until
//! every task ran, reported as µs/task.
//!
//! This is the phase where the TaskArena admission defect lived: every
//! `insert_external` scanned the entire free structure (65,536 entries at
//! arena creation), so admission cost was O(|free|) per task — invisible to
//! the 5 s keep-alive smoke, dominant in spawn storms. The tokio control
//! runs the same shape on a current-thread runtime + LocalSet.
//!
//! No sockets, no listener processes, no background workloads: the only
//! processes involved are this one. Output is measurement only; there is no
//! pass-line assert here (the O(1) admission regression guard is the
//! deterministic scan-counter assertion in `src/waker.rs` tests).

use rust_miniss::MultiCoreRuntime;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SPAWN_BASE_TASKS: usize = 20_000;
const COMPLETION_DEADLINE: Duration = Duration::from_secs(60);

fn main() {
    let miniss_us = miniss_spawn_baseline(SPAWN_BASE_TASKS);
    let tokio_us = tokio_spawn_baseline(SPAWN_BASE_TASKS);
    println!(
        "spawn_admission_storm ({} tasks per side)",
        SPAWN_BASE_TASKS
    );
    println!("  miniss spawn_local: {:>9.3} µs/task", miniss_us);
    println!("  tokio spawn_local:  {:>9.3} µs/task", tokio_us);
    println!("  ratio miniss/tokio: {:>9.1}x", miniss_us / tokio_us);
}

/// Same methodology as T3 `spawn_baseline`: spawn + poll + drop of an
/// immediately-completing task, measured from spawn start until every task
/// ran, on a one-shard runtime.
fn miniss_spawn_baseline(spawns: usize) -> f64 {
    let runtime = MultiCoreRuntime::new(Some(1)).expect("one-shard runtime");
    let (tx, rx) = std::sync::mpsc::channel();
    let runtime_for_bench = Arc::clone(&runtime);
    runtime
        .spawn_on(0, {
            let tx = tx.clone();
            async move {
                let completed = Arc::new(AtomicUsize::new(0));
                let start = Instant::now();
                for _ in 0..spawns {
                    let completed = Arc::clone(&completed);
                    runtime_for_bench
                        .spawn_local(async move {
                            completed.fetch_add(1, Ordering::Relaxed);
                        })
                        .expect("spawn baseline task");
                }
                // Await completion: the spawned tasks run on this same shard,
                // so the wait must yield to the executor rather than spin the
                // thread.
                wait_bounded(&completed, spawns, start, "miniss").await;
                let us_per_task = start.elapsed().as_secs_f64() * 1e6 / spawns as f64;
                let _ = tx.send(us_per_task);
            }
        })
        .expect("admit bench task");
    drop(tx);
    let us_per_task = rx
        .recv_timeout(COMPLETION_DEADLINE)
        .expect("miniss spawn baseline completed");
    runtime
        .shutdown_with_timeout(Duration::from_secs(10))
        .expect("runtime shutdown");
    us_per_task
}

/// Tokio control for the same shape: 20,000 `spawn_local` spawns of an
/// immediately-completing task on a current-thread runtime + LocalSet, timed
/// from spawn start until every task ran.
fn tokio_spawn_baseline(spawns: usize) -> f64 {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio current-thread runtime");
    let local = tokio::task::LocalSet::new();
    let us_per_task = local.block_on(&rt, async move {
        let completed = Arc::new(AtomicUsize::new(0));
        let start = Instant::now();
        for _ in 0..spawns {
            let completed = Arc::clone(&completed);
            tokio::task::spawn_local(async move {
                completed.fetch_add(1, Ordering::Relaxed);
            });
        }
        while completed.load(Ordering::Relaxed) < spawns {
            tokio::task::yield_now().await;
        }
        assert_deadline(start, "tokio");
        start.elapsed().as_secs_f64() * 1e6 / spawns as f64
    });
    us_per_task
}

async fn wait_bounded(completed: &AtomicUsize, spawns: usize, start: Instant, label: &str) {
    while completed.load(Ordering::Relaxed) < spawns {
        assert_deadline(start, label);
        // Yield through the runtime timer (never block the worker thread):
        // the spawned tasks run on this same shard and need an executor turn
        // to make progress.
        rust_miniss::timer::sleep(Duration::from_micros(200)).await;
    }
}

fn assert_deadline(start: Instant, label: &str) {
    assert!(
        start.elapsed() < COMPLETION_DEADLINE,
        "{label} spawn baseline did not drain within {COMPLETION_DEADLINE:?}"
    );
}
