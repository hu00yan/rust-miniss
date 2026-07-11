//! io_uring NOP throughput benchmark — batch-submit mode.
//!
//! Submits NOPs in bulk from a single task, flushes once, then drains all
//! completions. Measures the pure io_uring SQE→CQE path without per-NOP
//! task spawn overhead.
//!
//! Target: 10M+ NOPs/sec on modern hardware.

use futures::future::poll_fn;
use rust_miniss::MultiCoreRuntime;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::task::Poll;
use std::time::Instant;

fn main() {
    let runtime = MultiCoreRuntime::new(Some(1))
        .expect("Failed to create runtime (try on Linux with io_uring)");

    let done = Arc::new(AtomicBool::new(false));
    let d = done.clone();
    let (result_tx, result_rx) = mpsc::sync_channel(1);

    // Use a fixed batch size that works well
    let batch = 65536;
    let iterations = 1;

    runtime
        .spawn(async move {
            rust_miniss::cpu::with_io_state(|state| {
                // Warmup: 2 batches
                for _ in 0..2 {
                    for _ in 0..batch {
                        let _ = state.io_backend.submit(rust_miniss::io::Op::Nop);
                    }
                    state.io_backend.flush();
                    let mut remaining: usize = batch;
                    while remaining > 0 {
                        match state.io_backend.drain_completions() {
                            std::task::Poll::Ready(cqes) => {
                                remaining = remaining.saturating_sub(cqes.len());
                            }
                            std::task::Poll::Pending => {
                                state.io_backend.flush();
                                state.io_backend.park(std::time::Duration::from_millis(1));
                            }
                        }
                    }
                }

                // Measured run
                let total = batch * iterations;
                let start = Instant::now();

                for _ in 0..iterations {
                    for _ in 0..batch {
                        let _ = state.io_backend.submit(rust_miniss::io::Op::Nop);
                    }
                    state.io_backend.flush();
                    let mut remaining: usize = batch;
                    while remaining > 0 {
                        match state.io_backend.drain_completions() {
                            std::task::Poll::Ready(cqes) => {
                                remaining = remaining.saturating_sub(cqes.len());
                            }
                            std::task::Poll::Pending => {
                                state.io_backend.flush();
                                state.io_backend.park(std::time::Duration::from_millis(1));
                            }
                        }
                    }
                }

                let elapsed = start.elapsed();
                let ops = total as f64 / elapsed.as_secs_f64();
                let ns = elapsed.as_nanos() as f64 / total as f64;

                println!(
                    "batch={batch}  {total} NOPs  {:.4}s  {ops:>10.0} NOPs/sec  {ns:>6.0} ns/op",
                    elapsed.as_secs_f64(),
                );

                let _ = result_tx.send(ops);
                d.store(true, Ordering::Release);
            });
        })
        .ok();

    // Wait for task to complete using poll_fn (proper yielding)
    runtime.block_on(poll_fn(|cx| {
        if done.load(Ordering::Acquire) {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }));

    let ops = result_rx.recv().unwrap_or(0.0);
    println!("Throughput: {ops:.0} NOPs/sec");
    runtime.shutdown().unwrap();
}
