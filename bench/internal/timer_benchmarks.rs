use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use futures::executor::block_on;
use rust_miniss::multicore::MultiCoreRuntime;
use rust_miniss::timer::{Interval, SleepFuture, TimerWheel};
use std::sync::Arc;
use std::task::{Wake, Waker};
use std::time::{Duration, Instant};

struct BenchWaker;

impl Wake for BenchWaker {
    fn wake(self: Arc<Self>) {}
}

fn create_bench_waker() -> Waker {
    Arc::new(BenchWaker).into()
}

fn timer_insertion_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_insertion");
    group.throughput(Throughput::Elements(1));

    group.bench_function("single_insert", |b| {
        let mut wheel = TimerWheel::new(4096, 1);
        let now = Instant::now();

        b.iter(|| {
            let waker = create_bench_waker();
            let future_time = black_box(now + Duration::from_millis(100));
            let _timer_id = black_box(wheel.schedule(future_time, waker));
        })
    });

    group.finish();
}

fn timer_batch_insertion_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_batch_insertion");

    for batch_size in [10, 100, 1000].iter() {
        group.throughput(Throughput::Elements(*batch_size));
        group.bench_with_input(format!("batch_{}", batch_size), batch_size, |b, &size| {
            b.iter(|| {
                let mut wheel = TimerWheel::new(4096, 1);
                let now = Instant::now();

                for i in 0..size {
                    let waker = create_bench_waker();
                    let future_time = black_box(now + Duration::from_millis(i + 1));
                    let _timer_id = black_box(wheel.schedule(future_time, waker));
                }
            })
        });
    }

    group.finish();
}

fn timer_cancellation_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_cancellation");
    group.throughput(Throughput::Elements(1));

    group.bench_function("cancel_timer", |b| {
        b.iter_batched(
            || {
                let mut wheel = TimerWheel::new(4096, 1);
                let waker = create_bench_waker();
                let future_time = Instant::now() + Duration::from_millis(100);
                let timer_id = wheel.schedule(future_time, waker);
                (wheel, timer_id)
            },
            |(mut wheel, timer_id)| {
                let result = black_box(wheel.cancel(timer_id));
                black_box(result);
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.finish();
}

fn timer_expiration_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_expiration");

    for timer_count in [10, 100, 1000].iter() {
        group.throughput(Throughput::Elements(*timer_count));
        group.bench_with_input(
            format!("expire_{}_timers", timer_count),
            timer_count,
            |b, &count| {
                b.iter_batched(
                    || {
                        let mut wheel = TimerWheel::new(4096, 1);
                        let now = Instant::now();

                        // Schedule timers that should all expire
                        for i in 0..count {
                            let waker = create_bench_waker();
                            let timer_time = now + Duration::from_millis(1 + (i % 10));
                            wheel.schedule(timer_time, waker);
                        }

                        (wheel, now + Duration::from_millis(20))
                    },
                    |(mut wheel, expire_time)| {
                        let mut ready = Vec::new();
                        wheel.expire(expire_time, &mut ready);
                        black_box(ready);
                    },
                    criterion::BatchSize::SmallInput,
                )
            },
        );
    }

    group.finish();
}

fn timer_wheel_creation_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_wheel_creation");

    for num_slots in [256, 1024, 4096, 16384].iter() {
        group.bench_with_input(
            format!("create_{}_slots", num_slots),
            num_slots,
            |b, &slots| {
                b.iter(|| {
                    let _wheel = black_box(TimerWheel::new(slots, 1));
                })
            },
        );
    }

    group.finish();
}

fn timer_sleep_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_sleep");
    group.throughput(Throughput::Elements(1));

    group.bench_function("sleep_future", |b| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();
        let runtime = MultiCoreRuntime::new(Some(1)).unwrap();
        b.iter(|| {
            runtime.block_on(SleepFuture::new(Duration::from_millis(10)));
        })
    });

    group.finish();
}

fn interval_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("interval");
    group.throughput(Throughput::Elements(1));

    group.bench_function("interval_tick", |b| {
        b.iter(|| {
            let mut interval = Interval::new(Duration::from_millis(10));
            block_on(interval.tick());
        })
    });

    group.finish();
}

criterion_group!(
    timer_benches,
    timer_insertion_benchmark,
    timer_batch_insertion_benchmark,
    timer_cancellation_benchmark,
    timer_expiration_benchmark,
    timer_wheel_creation_benchmark,
    timer_sleep_benchmark,
    interval_benchmark
);
criterion_main!(timer_benches);
