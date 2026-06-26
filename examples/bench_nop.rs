//! io_uring NOP throughput benchmark.
//!
//! Measures the pure ring management overhead (SQE fill + submit + CQE reap)
//! by submitting IORING_OP_NOP operations and measuring throughput.
//!
//! Each NOP goes through the full submit → flush → complete cycle via the
//! runtime's event loop. A healthy result on modern hardware is 10M+ NOPs/sec.

use rust_miniss::MultiCoreRuntime;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let iterations: u64 = 50_000;
    let runtime = MultiCoreRuntime::new(Some(1))
        .expect("Failed to create runtime (try on Linux with io_uring)");

    let completed = Arc::new(AtomicU64::new(0));
    let start = Instant::now();

    // Spawn tasks that submit NOP operations via IoFuture
    for _ in 0..iterations {
        let c = completed.clone();
        runtime
            .spawn(async move {
                let state = rust_miniss::cpu::io_state();
                let token = state.io_backend.submit(rust_miniss::io::Op::Nop);
                let future = rust_miniss::io::future::IoFuture::new(token);
                let _ = future.await;
                c.fetch_add(1, Ordering::Relaxed);
            })
            .ok();
    }

    // Wait for all completions
    runtime.block_on(async {
        while completed.load(Ordering::Relaxed) < iterations {
            std::hint::spin_loop();
        }
    });

    let elapsed = start.elapsed();
    let ops = iterations as f64 / elapsed.as_secs_f64();
    let ns = elapsed.as_nanos() as f64 / iterations as f64;

    println!("═══════════════════════════════════════════");
    println!("  NOP Throughput Benchmark (io_uring)");
    println!("═══════════════════════════════════════════");
    println!("  Iterations:  {iterations}");
    println!("  Elapsed:     {:.4}s", elapsed.as_secs_f64());
    println!("  Throughput:  {:.0} NOPs/sec", ops);
    println!("  Latency:     {:.0} ns/op", ns);
    println!("═══════════════════════════════════════════");

    runtime.shutdown().unwrap();
}
