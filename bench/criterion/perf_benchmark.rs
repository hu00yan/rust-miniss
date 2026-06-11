use criterion::{criterion_group, criterion_main, Criterion};
use rust_miniss::{timer, Runtime};
use std::time::Duration;

fn bench_task_sleep(c: &mut Criterion) {
    let runtime = Runtime::new();
    c.bench_function("task_sleep_1us", |b| {
        b.iter(|| {
            runtime.block_on(async {
                timer::sleep(Duration::from_micros(1)).await;
            });
        });
    });
}

fn bench_task_chain(c: &mut Criterion) {
    let runtime = Runtime::new();
    c.bench_function("task_chain_small", |b| {
        b.iter(|| {
            runtime.block_on(async {
                let mut acc = 0u64;
                for i in 0..16 {
                    acc += i;
                    timer::sleep(Duration::from_nanos(50)).await;
                }
                criterion::black_box(acc);
            });
        });
    });
}

criterion_group!(benches, bench_task_sleep, bench_task_chain);
criterion_main!(benches);
