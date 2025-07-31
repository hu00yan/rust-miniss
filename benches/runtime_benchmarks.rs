use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rust_miniss::Runtime;

fn runtime_creation_benchmark(c: &mut Criterion) {
    c.bench_function("runtime_creation", |b| {
        b.iter(|| {
            let _runtime = black_box(Runtime::new());
        })
    });
}

fn basic_task_spawning_benchmark(c: &mut Criterion) {
    c.bench_function("basic_task_spawn", |b| {
        b.iter(|| {
            let runtime = Runtime::new();
            let _handle = black_box(runtime.spawn(async {
                // Simple async task
                42
            }));
        })
    });
}

criterion_group!(
    benches,
    runtime_creation_benchmark,
    basic_task_spawning_benchmark
);
criterion_main!(benches);
