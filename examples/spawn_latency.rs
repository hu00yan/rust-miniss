//! Task spawn + wakeup roundtrip latency benchmark.
//! Measures pure scheduling overhead: spawn → poll → complete.

use rust_miniss::MultiCoreRuntime;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let iterations: u64 = 100_000;
    let runtime = MultiCoreRuntime::new(Some(1)).unwrap();

    let counter = Arc::new(AtomicU64::new(0));

    let start = Instant::now();

    // Spawn tasks that immediately complete
    for _ in 0..iterations {
        let c = counter.clone();
        runtime
            .spawn(async move {
                c.fetch_add(1, Ordering::Relaxed);
            })
            .ok();
    }

    // Block until all tasks complete
    runtime.block_on(async move {
        while counter.load(Ordering::Relaxed) < iterations {
            rust_miniss::timer::sleep(std::time::Duration::from_micros(10)).await;
        }
    });

    let elapsed = start.elapsed();
    let ops_per_sec = iterations as f64 / elapsed.as_secs_f64();
    let ns_per_op = elapsed.as_nanos() as f64 / iterations as f64;

    println!("Task spawn + wakeup latency:");
    println!("  Total tasks: {iterations}");
    println!("  Elapsed: {:.3}s", elapsed.as_secs_f64());
    println!("  Tasks/sec: {ops_per_sec:.0}");
    println!("  ns/task: {ns_per_op:.0}");
}
