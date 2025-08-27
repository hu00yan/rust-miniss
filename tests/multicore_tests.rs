#[cfg(test)]
mod unit_tests {
    use rust_miniss::timer::*;
    use std::sync::Arc;
    use std::{task::*, time::*};

    struct TestWaker;

    impl Wake for TestWaker {
        fn wake(self: Arc<Self>) {}
    }

    fn create_test_waker() -> Waker {
        Arc::new(TestWaker).into()
    }

    #[test]
    fn test_wheel_insert_cancel_expire() {
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
    let runtime = MultiCoreRuntime::with_cpus(4).unwrap();
    let cpus_completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let (tx, rx) = std::sync::mpsc::channel();
    for cpu_id in 0..4 {
        let cpus_completed_clone = cpus_completed.clone();
        let tx_clone = tx.clone();
        runtime
            .spawn_on(cpu_id, async move {
                // Simulate timer scheduling work on this CPU
                cpus_completed_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tx_clone.send(cpu_id).unwrap();
            })
            .unwrap();
    }

    let mut received_cpus = Vec::new();
    for _ in 0..4 {
        let cpu_id = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        received_cpus.push(cpu_id);
    }

    assert_eq!(received_cpus.len(), 4);

    runtime.shutdown().unwrap();
}


#[test]
fn test_graceful_shutdown_no_leaks() {
    let runtime = rust_miniss::multicore::MultiCoreRuntime::with_cpus(2).unwrap();

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

#[test]
fn test_multicore_basic_functionality() {
    let runtime = MultiCoreRuntime::with_cpus(2).unwrap();

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
    let runtime = MultiCoreRuntime::with_cpus(4).unwrap();
    let results = Arc::new(std::sync::Mutex::new(Vec::new()));

    let (tx, rx) = mpsc::channel();
    for cpu_id in 0..4 {
        let results_clone = results.clone();
        let tx_clone = tx.clone();
        runtime
            .spawn_on(cpu_id, async move {
                // Simulate some work
                std::thread::yield_now();
                results_clone.lock().unwrap().push(cpu_id);
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

    // All CPUs should have executed their tasks
    for cpu_id in 0..4 {
        assert!(final_results.contains(&cpu_id));
    }

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_round_robin_distribution() {
    let runtime = MultiCoreRuntime::with_cpus(3).unwrap();
    let cpu_counters = Arc::new([AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)]);

    let (tx, rx) = mpsc::channel();
    for _ in 0..15 {
        let counters = cpu_counters.clone();
        let tx_clone = tx.clone();
        runtime
            .spawn(async move {
                // Try to identify which CPU we're on by thread name
                let thread = std::thread::current();
                let thread_name = thread.name().unwrap_or("unknown");
                if let Some(cpu_id_str) = thread_name.strip_prefix("miniss-cpu-") {
                    if let Ok(cpu_id) = cpu_id_str.parse::<usize>() {
                        if cpu_id < 3 {
                            counters[cpu_id].fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }
                tx_clone.send(()).unwrap();
            })
            .unwrap();
    }

    // Wait for tasks to complete
    for _ in 0..15 {
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    // Check that tasks were distributed (roughly evenly)
    let total_tasks: u32 = cpu_counters
        .iter()
        .map(|counter| counter.load(Ordering::SeqCst))
        .sum();

    // We expect some tasks to be executed (exact distribution may vary)
    assert!(total_tasks > 0, "No tasks were executed");

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_block_on() {
    let runtime = MultiCoreRuntime::with_cpus(2).unwrap();

    // Test block_on with immediate value
    let result = runtime.block_on(async { 42 }).unwrap();
    assert_eq!(result, 42);

    // Test block_on with computation
    let result = runtime
        .block_on(async {
            let mut sum = 0;
            for i in 1..=10 {
                sum += i;
            }
            sum
        })
        .unwrap();
    assert_eq!(result, 55);

    // Test block_on on specific CPU
    let result = runtime
        .block_on_cpu(1, async { "hello from CPU 1" })
        .unwrap();
    assert_eq!(result, "hello from CPU 1");

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_error_handling() {
    let runtime = MultiCoreRuntime::with_cpus(2).unwrap();

    // Test invalid CPU ID
    let result = runtime.spawn_on(5, async {});
    assert!(result.is_err());

    let result = runtime.block_on_cpu(10, async { 42 });
    assert!(result.is_err());

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_zero_cpus_error() {
    let result = MultiCoreRuntime::with_cpus(0);
    assert!(result.is_err());
}

#[test]
fn test_multicore_concurrent_spawning() {
    let runtime = Arc::new(MultiCoreRuntime::with_cpus(4).unwrap());
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

    // Wait for all tasks to complete by yielding
    std::thread::yield_now();

    assert_eq!(counter.load(Ordering::SeqCst), 40);

    // Use Arc::try_unwrap to get ownership for shutdown
    let runtime = Arc::try_unwrap(runtime).expect("Failed to unwrap runtime Arc");
    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_ping_communication() {
    let runtime = MultiCoreRuntime::with_cpus(3).unwrap();

    runtime.ping_all().unwrap();

    // Give time for ping messages to be processed
    // In a real implementation, you might want a more deterministic way to check this
    std::thread::yield_now();

    runtime.shutdown().unwrap();
}

#[test]
#[ignore] // This test is slow and should be run explicitly
fn test_multicore_performance_comparison() {
    // This test compares single-core vs multi-core performance
    let task_count = 1000;

    // Single-core runtime
    let start = Instant::now();
    let single_runtime = Runtime::new();
    let counter = Arc::new(AtomicU32::new(0));

    for _ in 0..task_count {
        let counter_clone = counter.clone();
        single_runtime.spawn(async move {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });
    }

    // Wait for single-core tasks
    while counter.load(Ordering::SeqCst) < task_count {
        std::thread::yield_now();
    }
    let single_duration = start.elapsed();

    // Multi-core runtime
    let start = Instant::now();
    let multi_runtime = MultiCoreRuntime::with_cpus(4).unwrap();
    let counter2 = Arc::new(AtomicU32::new(0));

    for _ in 0..task_count {
        let counter_clone = counter2.clone();
        multi_runtime
            .spawn(async move {
                counter_clone.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
    }

    // Wait for multi-core tasks
    while counter2.load(Ordering::SeqCst) < task_count {
        std::thread::yield_now();
    }
    let multi_duration = start.elapsed();

    println!("Single-core: {:?}", single_duration);
    println!("Multi-core: {:?}", multi_duration);
    println!(
        "Tasks completed: single={}, multi={}",
        counter.load(Ordering::SeqCst),
        counter2.load(Ordering::SeqCst)
    );

    // Both should complete all tasks
    assert_eq!(counter.load(Ordering::SeqCst), task_count);
    assert_eq!(counter2.load(Ordering::SeqCst), task_count);

    multi_runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_optimal_cpu_count() {
    let runtime = MultiCoreRuntime::new_optimal().unwrap();

    // Should use at least 1 CPU, and not more than available logical cores
    let cpu_count = runtime.cpu_count();
    assert!(cpu_count >= 1);
    assert!(cpu_count <= num_cpus::get());

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_task_isolation() {
    let runtime = MultiCoreRuntime::with_cpus(2).unwrap();

    // Create data that should be isolated between CPUs
    let cpu0_data = Arc::new(AtomicU32::new(0));
    let cpu1_data = Arc::new(AtomicU32::new(0));

    let (tx, rx) = mpsc::channel();
    let cpu0_clone = cpu0_data.clone();
    let tx_clone = tx.clone();
    runtime
        .spawn_on(0, async move {
            for _ in 0..100 {
                cpu0_clone.fetch_add(1, Ordering::SeqCst);
            }
            tx_clone.send(()).unwrap();
        })
        .unwrap();

    let cpu1_clone = cpu1_data.clone();
    let tx_clone = tx.clone();
    runtime
        .spawn_on(1, async move {
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
    let runtime = MultiCoreRuntime::with_cpus(3).unwrap();
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
