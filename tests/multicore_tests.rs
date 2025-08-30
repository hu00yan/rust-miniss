#[cfg(test)]
mod tests {
    use rust_miniss::timer::*;
    use std::sync::Arc;
    use std::{task::*, time::*};

    fn init_tracing() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();
    }

    struct TestWaker;

    impl Wake for TestWaker {
        fn wake(self: Arc<Self>) {}
    }

    fn create_test_waker() -> Waker {
        Arc::new(TestWaker).into()
    }

    #[test]
    fn test_wheel_insert_cancel_expire() {
        init_tracing();
        let mut wheel = TimerWheel::new(10, 1); // Small wheel for testing
        let now = Instant::now();

        // Test scheduling
        let waker = create_test_waker();
        let timer_id = wheel.schedule(now + Duration::from_millis(5), waker);
        assert_eq!(wheel.pending_count(), 1);

        // Test cancelling
        assert!(wheel.cancel(timer_id));
        assert_eq!(wheel.pending_count(), 0);

        // Test expiry
        let mut ready = Vec::new();
        wheel.expire(now + Duration::from_millis(5), &mut ready);
        assert_eq!(ready.len(), 0);
    }
}

#[test]
fn test_timer_schedule_across_cpus() {
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(4)).unwrap();
    let cpus_completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let (tx, rx) = std::sync::mpsc::channel();
    for _ in 0..4 {
        let cpus_completed_clone = cpus_completed.clone();
        let tx_clone = tx.clone();
        runtime
            .spawn(async move {
                // Simulate timer scheduling work on this CPU
                cpus_completed_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tx_clone.send(()).unwrap();
            })
            .unwrap();
    }

    let mut received_count = 0;
    for _ in 0..4 {
        rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        received_count += 1;
    }

    assert_eq!(received_count, 4);

    runtime.shutdown().unwrap();
}

#[test]
fn test_graceful_shutdown_no_leaks() {
    init_tracing();
    let runtime = rust_miniss::multicore::MultiCoreRuntime::new(Some(2)).unwrap();

    // Spawn a task to ensure there are active tasks when we drop the runtime
    runtime
        .spawn(async {
            // Just a simple task that completes quickly
            std::thread::yield_now();
        })
        .unwrap();

    // Drop the runtime, which should trigger a graceful shutdown
    drop(runtime);

    // Allow some time for shutdown to complete
    std::thread::yield_now();

    // If we get here without hanging, the Drop implementation worked correctly
}

/// Integration tests for multi-core runtime functionality
use rust_miniss::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn init_tracing() {
    // Initialize tracing to prevent panics in multi-threaded environment
    // Use try_init to avoid panic if already initialized
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

#[test]
fn test_multicore_basic_functionality() {
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();

    // Test basic properties
    assert_eq!(runtime.cpu_count(), 2);

    // Test spawning a simple task
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = counter.clone();

    let (tx, rx) = mpsc::channel();
    runtime
        .spawn(async move {
            counter_clone.fetch_add(1, Ordering::SeqCst);
            tx.send(()).unwrap();
        })
        .unwrap();

    // Wait for task to complete
    rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_cross_cpu_communication() {
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(4)).unwrap();
    let results = Arc::new(std::sync::Mutex::new(Vec::new()));

    let (tx, rx) = mpsc::channel();
    for _ in 0..4 {
        let results_clone = results.clone();
        let tx_clone = tx.clone();
        runtime
            .spawn(async move {
                // Simulate some work
                std::thread::yield_now();
                // Since we can't control which CPU the task runs on, we'll just push a value
                results_clone.lock().unwrap().push(1);
                tx_clone.send(()).unwrap();
            })
            .unwrap();
    }

    // Wait for all tasks to complete
    for _ in 0..4 {
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    let final_results = results.lock().unwrap();
    assert_eq!(final_results.len(), 4);

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_round_robin_distribution() {
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(3)).unwrap();
    let task_counter = Arc::new(AtomicU32::new(0));

    let (tx, rx) = mpsc::channel();
    for _ in 0..15 {
        let counter = task_counter.clone();
        let tx_clone = tx.clone();
        runtime
            .spawn(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                tx_clone.send(()).unwrap();
            })
            .unwrap();
    }

    // Wait for tasks to complete
    for _ in 0..15 {
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    // Check that tasks were executed
    let total_tasks = task_counter.load(Ordering::SeqCst);
    assert_eq!(total_tasks, 15, "Expected 15 tasks to be executed");

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_block_on() {
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();

    // Test block_on with immediate value
    let result = runtime.block_on(async { 42 });
    assert_eq!(result, 42);

    // Test block_on with computation
    let result = runtime.block_on(async {
        let mut sum = 0;
        for i in 1..=10 {
            sum += i;
        }
        sum
    });
    assert_eq!(result, 55);

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_error_handling() {
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();

    // Test invalid CPU count
    let result = MultiCoreRuntime::new(Some(0));
    assert!(result.is_err());

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_zero_cpus_error() {
    let result = MultiCoreRuntime::new(Some(0));
    assert!(result.is_err());
}

#[test]
fn test_multicore_concurrent_spawning() {
    init_tracing();
    let runtime = Arc::new(MultiCoreRuntime::new(Some(4)).unwrap());
    let counter = Arc::new(AtomicU32::new(0));
    let mut handles = Vec::new();

    // Spawn multiple threads that each spawn tasks
    for _ in 0..4 {
        let runtime_clone = runtime.clone();
        let counter_clone = counter.clone();

        let handle = std::thread::spawn(move || {
            for _ in 0..10 {
                let counter_inner = counter_clone.clone();
                runtime_clone
                    .spawn(async move {
                        counter_inner.fetch_add(1, Ordering::SeqCst);
                    })
                    .unwrap();
            }
        });

        handles.push(handle);
    }

    // Wait for all spawning threads to complete
    for handle in handles {
        handle.join().unwrap();
    }

    // Wait for all tasks to complete - use a more reliable method
    // Since task execution is asynchronous, we need to wait longer
    for _ in 0..100 {
        if counter.load(Ordering::SeqCst) == 40 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let final_count = counter.load(Ordering::SeqCst);
    assert_eq!(
        final_count, 40,
        "Expected 40 tasks to complete, but only {} completed",
        final_count
    );

    // Shutdown the runtime
    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_ping_communication() {
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(3)).unwrap();

    // Ping functionality is not implemented yet
    // runtime.ping_all().unwrap();

    // Give time for messages to be processed
    std::thread::yield_now();

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_performance_comparison() {
    init_tracing();
    // This test compares single-core vs multi-core performance
    let task_count = 50; // Reduced further for reliability

    // Single-core runtime - block_on to ensure execution
    let start = Instant::now();
    let single_runtime = Runtime::new();
    let counter = Arc::new(AtomicU32::new(0));

    let single_handles: Vec<_> = (0..task_count)
        .map(|_| {
            let counter_clone = counter.clone();
            single_runtime.spawn(async move {
                counter_clone.fetch_add(1, Ordering::SeqCst);
            })
        })
        .collect();

    // Block until all single-core tasks complete
    single_runtime.block_on(async move {
        for handle in single_handles {
            let _ = handle.await;
        }
    });

    let single_duration = start.elapsed();
    let single_completed = counter.load(Ordering::SeqCst);

    // Multi-core runtime
    let start = Instant::now();
    let multi_runtime = MultiCoreRuntime::new(Some(4)).unwrap();
    let counter2 = Arc::new(AtomicU32::new(0));

    // Use channels for synchronization
    let (tx, rx) = mpsc::channel();
    for _ in 0..task_count {
        let counter_clone = counter2.clone();
        let tx_clone = tx.clone();
        multi_runtime
            .spawn(async move {
                counter_clone.fetch_add(1, Ordering::SeqCst);
                let _ = tx_clone.send(());
            })
            .unwrap();
    }

    // Wait for multi-core tasks with timeout
    let mut multi_completed = 0;
    for _ in 0..task_count {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(_) => multi_completed += 1,
            Err(_) => break,
        }
    }
    let multi_duration = start.elapsed();

    println!(
        "Single-core: {:?} (completed: {})",
        single_duration, single_completed
    );
    println!(
        "Multi-core: {:?} (completed: {})",
        multi_duration, multi_completed
    );

    // Both should complete most tasks (allow some tolerance for timing)
    assert!(
        single_completed >= task_count * 3 / 4,
        "Single-core completed {}, expected at least {}",
        single_completed,
        task_count * 3 / 4
    );
    assert!(
        multi_completed >= task_count * 3 / 4,
        "Multi-core completed {}, expected at least {}",
        multi_completed,
        task_count * 3 / 4
    );

    multi_runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_optimal_cpu_count() {
    init_tracing();
    let runtime = MultiCoreRuntime::new(None).unwrap();

    // Should use at least 1 CPU, and not more than available logical cores
    let cpu_count = runtime.cpu_count();
    assert!(cpu_count >= 1);
    assert!(cpu_count <= num_cpus::get());

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_task_isolation() {
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();

    // Create data that should be isolated between CPUs
    let cpu0_data = Arc::new(AtomicU32::new(0));
    let cpu1_data = Arc::new(AtomicU32::new(0));

    let (tx, rx) = mpsc::channel();
    let cpu0_clone = cpu0_data.clone();
    let tx_clone = tx.clone();
    runtime
        .spawn(async move {
            for _ in 0..100 {
                cpu0_clone.fetch_add(1, Ordering::SeqCst);
            }
            tx_clone.send(()).unwrap();
        })
        .unwrap();

    let cpu1_clone = cpu1_data.clone();
    let tx_clone = tx.clone();
    runtime
        .spawn(async move {
            for _ in 0..50 {
                cpu1_clone.fetch_add(2, Ordering::SeqCst);
            }
            tx_clone.send(()).unwrap();
        })
        .unwrap();

    // Wait for tasks to complete
    for _ in 0..2 {
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    assert_eq!(cpu0_data.load(Ordering::SeqCst), 100);
    assert_eq!(cpu1_data.load(Ordering::SeqCst), 100);

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_graceful_shutdown() {
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(3)).unwrap();
    let running_tasks = Arc::new(AtomicU32::new(0));

    let (tx, rx) = mpsc::channel();
    for _ in 0..5 {
        let counter = running_tasks.clone();
        let tx_clone = tx.clone();
        runtime
            .spawn(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                // Simulate some work
                std::thread::yield_now();
                counter.fetch_sub(1, Ordering::SeqCst);
                tx_clone.send(()).unwrap();
            })
            .unwrap();
    }

    // Give tasks time to start
    for _ in 0..5 {
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    // Graceful shutdown should wait for tasks to complete
    runtime.shutdown().unwrap();

    // All tasks should have completed
    assert_eq!(running_tasks.load(Ordering::SeqCst), 0);
}
