use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[test]
fn multicore_task_distribution() {
    let runtime = rust_miniss::MultiCoreRuntime::new(Some(4)).expect("create multicore runtime");

    let counters = Arc::new([
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ]);

    runtime.block_on(async {
        for i in 0..4 {
            let counters = counters.clone();
            runtime.spawn(async move {
                for _j in 0..250 {
                    counters[i].fetch_add(1, Ordering::Relaxed);
                }
            }).expect("spawn task");
        }

        rust_miniss::timer::sleep(std::time::Duration::from_millis(200)).await;
    });

    let counts: Vec<_> = counters.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    let total: usize = counts.iter().sum();
    assert_eq!(total, 1000, "expected 1000 total tasks, got {total} with counts {counts:?}");
}
