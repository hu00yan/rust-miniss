use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use rust_miniss::multicore::MultiCoreRuntime;
use std::sync::mpsc::sync_channel;
use std::time::Instant;

fn scheduling_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduling_throughput");

    for &task_count in &[1_000usize, 5_000, 10_000] {
        group.throughput(Throughput::Elements(task_count as u64));
        group.bench_with_input(
            format!("spawn_complete_{}", task_count),
            &task_count,
            |b, &n| {
                b.iter_custom(|iters| {
                    // Create a runtime per measurement to avoid interference
                    let runtime = MultiCoreRuntime::with_cpus(4).expect("runtime with 4 cpus");

                    // Warmup once
                    {
                        let (tx, rx) = sync_channel::<()>(n);
                        for _ in 0..n {
                            let txc = tx.clone();
                            runtime
                                .spawn(async move {
                                    let _ = txc.send(());
                                })
                                .expect("spawn");
                        }
                        for _ in 0..n {
                            let _ = rx.recv().unwrap();
                        }
                    }

                    let start = Instant::now();
                    for _ in 0..iters {
                        let (tx, rx) = sync_channel::<()>(n);
                        for _ in 0..n {
                            let txc = tx.clone();
                            runtime
                                .spawn(async move {
                                    let _ = txc.send(());
                                })
                                .expect("spawn");
                        }
                        for _ in 0..n {
                            let _ = rx.recv().unwrap();
                        }
                    }
                    let elapsed = start.elapsed();

                    // Drop runtime to shutdown threads between criterion measurements
                    drop(runtime);

                    elapsed
                })
            },
        );
    }

    group.finish();
}

criterion_group!(sched_benches, scheduling_throughput);
criterion_main!(sched_benches);
