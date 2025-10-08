use loom::thread;
use rust_miniss::{Runtime, timer};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn test_concurrent_tasks() {
    loom::model(|| {
        let runtime = Runtime::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        runtime.block_on(async move {
            let handles = (0..4).map(|_| {
                let counter = counter_clone.clone();
                async move {
                    timer::sleep(Duration::from_millis(1)).await;
                    counter.fetch_add(1, Ordering::Release);
                }
            });

            for handle in handles {
                handle.await;
            }
        });

        assert_eq!(counter.load(Ordering::Acquire), 4);
    });
}

#[test]
fn test_memory_order() {
    // Test Acquire/Release in task scheduling
    loom::model(|| {
        let flag = Arc::new(AtomicUsize::new(0));
        let flag_clone = flag.clone();

        let runtime = Runtime::new();
        runtime.block_on(async move {
            // Simulate cross-CPU message
            flag_clone.store(1, Ordering::Release);
            timer::sleep(Duration::from_millis(1)).await;
            assert_eq!(flag.load(Ordering::Acquire), 1);
        });
    });
}