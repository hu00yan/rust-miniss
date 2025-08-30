use criterion::{criterion_group, criterion_main, Criterion};
use rust_miniss::multicore::MultiCoreRuntime;
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::time::Instant;

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

// Measures one-way cross-CPU message latency by having a task on CPU 0
// submit a task to CPU 1 which signals completion back to the main thread.
fn cross_cpu_one_way_latency(c: &mut Criterion) {
    init_tracing();
    let runtime = Arc::new(MultiCoreRuntime::with_cpus(2).expect("runtime with 2 CPUs"));

    c.bench_function("cross_cpu_one_way_latency", |b| {
        b.iter_custom(|iters| {
            // Warmup single iteration outside of timing to prime threads
            {
                let (tx, rx) = sync_channel::<()>(0);
                let rt = runtime.clone();
                rt.spawn_on(0, {
                    let rt2 = rt.clone();
                    let tx2 = tx.clone();
                    async move {
                        // Submit a tiny task to CPU 1 that acks back
                        rt2.spawn_on(1, async move {
                            let _ = tx2.send(());
                        })
                        .expect("spawn_on cpu1");
                    }
                })
                .expect("spawn_on cpu0");
                rx.recv().unwrap();
            }

            let start = Instant::now();
            for _ in 0..iters {
                let (tx, rx) = sync_channel::<()>(0);
                let rt = runtime.clone();
                rt.spawn_on(0, {
                    let rt2 = rt.clone();
                    let tx2 = tx.clone();
                    async move {
                        rt2.spawn_on(1, async move {
                            let _ = tx2.send(());
                        })
                        .expect("spawn_on cpu1");
                    }
                })
                .expect("spawn_on cpu0");
                // Wait until CPU 1 runs the tiny task
                rx.recv().unwrap();
            }
            start.elapsed()
        })
    });

    // Ensure a clean shutdown after benchmarks
    // Drop the Arc to trigger Drop/Shutdown logic
    drop(runtime);
}

criterion_group!(cross_cpu_benches, cross_cpu_one_way_latency);
criterion_main!(cross_cpu_benches);
