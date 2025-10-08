use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rust_miniss::{Runtime, timer};
use std::time::Duration;

fn bench_task_creation(c: &mut Criterion) {
    let runtime = Runtime::new();
    c.bench_function("task_creation", |b| {
        b.iter(|| {
            runtime.block_on(async {
                timer::sleep(Duration::from_nanos(1)).await;
            });
        });
    });
}

fn bench_cross_cpu_latency(c: &mut Criterion) {
    // Simulate SPSC queue latency
    c.bench_function("cross_cpu_latency", |b| {
        b.iter(|| {
            // Placeholder for actual cross-CPU test
            black_box(1);
        });
    });
}

criterion_group!(benches, bench_task_creation, bench_cross_cpu_latency);
criterion_main!(benches);