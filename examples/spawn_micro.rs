use rust_miniss::MultiCoreRuntime;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let iterations: u64 = 100_000;
    let runtime = MultiCoreRuntime::new(Some(1)).unwrap();

    let counter = Arc::new(AtomicU64::new(0));

    // Phase 1: Measure pure spawn throughput (no waiting)
    let spawn_start = Instant::now();
    for _ in 0..iterations {
        let c = counter.clone();
        runtime
            .spawn(async move {
                c.fetch_add(1, Ordering::Relaxed);
            })
            .ok();
    }
    let spawn_elapsed = spawn_start.elapsed();

    // Phase 2: Wait for completion using a busy-wait on the core
    let wait_start = Instant::now();
    while counter.load(Ordering::Relaxed) < iterations {
        std::thread::yield_now();
    }
    let wait_elapsed = wait_start.elapsed();

    let total = spawn_start.elapsed();
    println!(
        "Spawn phase:   {:.3}s ({:.0} tasks/s)",
        spawn_elapsed.as_secs_f64(),
        iterations as f64 / spawn_elapsed.as_secs_f64()
    );
    println!(
        "Wait phase:    {:.3}s ({:.0} tasks/s)",
        wait_elapsed.as_secs_f64(),
        iterations as f64 / wait_elapsed.as_secs_f64()
    );
    println!(
        "Total:         {:.3}s ({:.0} tasks/s)",
        total.as_secs_f64(),
        iterations as f64 / total.as_secs_f64()
    );
    println!(
        "ns/task total: {:.0}",
        total.as_nanos() as f64 / iterations as f64
    );

    runtime.shutdown().unwrap();
}
