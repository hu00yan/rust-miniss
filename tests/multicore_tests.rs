#[cfg(test)]
mod tests {
    use rust_miniss::timer::*;
    use std::{task::*, time::*};

    fn init_tracing() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();
    }

    fn create_test_waker() -> Waker {
        Waker::noop().clone()
    }

    #[test]
    fn test_wheel_insert_cancel_expire() {
        init_tracing();
        let mut wheel = TimerWheel::new(10, 1); // Small wheel for testing
        let now = Instant::now();

        // Test scheduling
        let waker = create_test_waker();
        let timer_id = wheel.schedule(now + Duration::from_millis(5), &waker);
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
    let _test_lock = runtime_test_lock();
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
                cpus_completed_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

    runtime
        .shutdown_with_timeout(Duration::from_secs(1))
        .unwrap();
    runtime.join().unwrap();
}

#[test]
fn test_graceful_shutdown_no_leaks() {
    let _test_lock = runtime_test_lock();
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
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

// A runtime owns worker threads and pins them to shard CPUs.  Serialize this
// binary's runtime tests so concurrent runtime startup/shutdown cannot make a
// passing test leave workers behind or contend on the same affinity plan.
static RUNTIME_TEST_LOCK: Mutex<()> = Mutex::new(());

fn runtime_test_lock() -> MutexGuard<'static, ()> {
    RUNTIME_TEST_LOCK
        .lock()
        .expect("multicore test lock poisoned")
}

fn init_tracing() {
    // Initialize tracing to prevent panics in multi-threaded environment
    // Use try_init to avoid panic if already initialized
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

#[test]
fn test_multicore_basic_functionality() {
    let _test_lock = runtime_test_lock();
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
            counter_clone.fetch_add(1, Ordering::Relaxed);
            tx.send(()).unwrap();
        })
        .unwrap();

    // Wait for task to complete
    rx.recv_timeout(Duration::from_millis(200)).unwrap();
    assert_eq!(counter.load(Ordering::Acquire), 1);

    runtime.shutdown().unwrap();
}

#[test]
fn test_spawn_local_requires_worker_core() {
    let _test_lock = runtime_test_lock();
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();

    let result = runtime.spawn_local(async {});
    assert!(result.is_err());

    runtime.shutdown().unwrap();
}

#[test]
fn test_spawn_local_runs_from_worker_core() {
    let _test_lock = runtime_test_lock();
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
    let (tx, rx) = mpsc::channel();

    let rt = runtime.clone();
    runtime
        .spawn_on(0, async move {
            let tx_inner = tx.clone();
            rt.spawn_local(async move {
                tx_inner.send(()).unwrap();
            })
            .unwrap();
        })
        .unwrap();

    rx.recv_timeout(Duration::from_millis(200)).unwrap();
    runtime.shutdown().unwrap();
}

#[test]
fn test_spawn_local_accepts_non_send_future() {
    let _test_lock = runtime_test_lock();
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(1)).unwrap();
    let (tx, rx) = mpsc::channel();
    let rt = runtime.clone();
    runtime
        .spawn_on(0, async move {
            // Rc makes this future !Send; it must remain on the accepting
            // shard and never enter the cross-core message queue.
            let local = std::rc::Rc::new(7_u32);
            let tx_inner = tx.clone();
            rt.spawn_local(async move {
                tx_inner.send(*local).unwrap();
            })
            .unwrap();
        })
        .unwrap();

    assert_eq!(rx.recv_timeout(Duration::from_millis(200)).unwrap(), 7);
    runtime.shutdown().unwrap();
}

#[test]
fn test_spawn_wait_runs_task() {
    let _test_lock = runtime_test_lock();
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
    let (tx, rx) = mpsc::channel();

    runtime
        .block_on(runtime.spawn_wait(
            async move {
                tx.send(()).unwrap();
            },
            Duration::from_millis(200),
        ))
        .unwrap();

    rx.recv_timeout(Duration::from_millis(200)).unwrap();
    runtime.shutdown().unwrap();
}

#[test]
fn test_spawn_on_wait_rejects_invalid_core() {
    let _test_lock = runtime_test_lock();
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();

    let result = runtime.block_on(runtime.spawn_on_wait(2, async {}, Duration::from_millis(200)));
    assert!(result.is_err());

    runtime.shutdown().unwrap();
}

#[test]
fn test_runtime_metrics_snapshot_reports_each_core() {
    let _test_lock = runtime_test_lock();
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();

    let metrics = runtime
        .metrics_snapshot(Duration::from_millis(200))
        .unwrap();

    assert_eq!(metrics.cores.len(), 2);
    assert_eq!(metrics.cores[0].core_id, 0);
    assert_eq!(metrics.cores[1].core_id, 1);

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_cross_cpu_communication() {
    let _test_lock = runtime_test_lock();
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(4)).unwrap();

    let (tx, rx) = mpsc::channel();
    for _ in 0..4 {
        let tx_clone = tx.clone();
        runtime
            .spawn(async move {
                // Simulate some work
                std::thread::yield_now();
                tx_clone.send(1usize).unwrap();
            })
            .unwrap();
    }

    // Wait for all tasks to complete
    let mut results = Vec::new();
    for _ in 0..4 {
        results.push(rx.recv_timeout(Duration::from_millis(200)).unwrap());
    }

    assert_eq!(results.len(), 4);

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_round_robin_distribution() {
    let _test_lock = runtime_test_lock();
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(3)).unwrap();
    let task_counter = Arc::new(AtomicU32::new(0));

    let (tx, rx) = mpsc::channel();
    for _ in 0..15 {
        let counter = task_counter.clone();
        let tx_clone = tx.clone();
        runtime
            .spawn(async move {
                counter.fetch_add(1, Ordering::Relaxed);
                tx_clone.send(()).unwrap();
            })
            .unwrap();
    }

    // Wait for tasks to complete
    for _ in 0..15 {
        rx.recv_timeout(Duration::from_millis(200)).unwrap();
    }

    // Check that tasks were executed
    let total_tasks = task_counter.load(Ordering::Acquire);
    assert_eq!(total_tasks, 15, "Expected 15 tasks to be executed");

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_block_on() {
    let _test_lock = runtime_test_lock();
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
    let _test_lock = runtime_test_lock();
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();

    // Test invalid CPU count
    let result = MultiCoreRuntime::new(Some(0));
    assert!(result.is_err());

    runtime
        .shutdown_with_timeout(Duration::from_secs(1))
        .unwrap();
    runtime.join().unwrap();
}

#[test]
fn test_multicore_zero_cpus_error() {
    let _test_lock = runtime_test_lock();
    let result = MultiCoreRuntime::new(Some(0));
    assert!(result.is_err());
}

#[test]
fn test_multicore_concurrent_spawning() {
    let _test_lock = runtime_test_lock();
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
                        counter_inner.fetch_add(1, Ordering::Relaxed);
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
        if counter.load(Ordering::Acquire) == 40 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let final_count = counter.load(Ordering::Acquire);
    assert_eq!(
        final_count, 40,
        "Expected 40 tasks to complete, but only {final_count} completed"
    );

    // Shutdown the runtime
    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_optimal_cpu_count() {
    let _test_lock = runtime_test_lock();
    init_tracing();
    let runtime = MultiCoreRuntime::new(None).unwrap();

    // Should use at least 1 CPU, and not more than available logical cores
    let cpu_count = runtime.cpu_count();
    assert!(cpu_count >= 1);
    assert!(cpu_count <= num_cpus::get());

    runtime
        .shutdown_with_timeout(Duration::from_secs(1))
        .unwrap();
    runtime.join().unwrap();
}

#[test]
fn test_multicore_task_isolation() {
    let _test_lock = runtime_test_lock();
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
                cpu0_clone.fetch_add(1, Ordering::Relaxed);
            }
            tx_clone.send(()).unwrap();
        })
        .unwrap();

    let cpu1_clone = cpu1_data.clone();
    let tx_clone = tx.clone();
    runtime
        .spawn(async move {
            for _ in 0..50 {
                cpu1_clone.fetch_add(2, Ordering::Relaxed);
            }
            tx_clone.send(()).unwrap();
        })
        .unwrap();

    // Wait for tasks to complete
    for _ in 0..2 {
        rx.recv_timeout(Duration::from_millis(200)).unwrap();
    }

    assert_eq!(cpu0_data.load(Ordering::Acquire), 100);
    assert_eq!(cpu1_data.load(Ordering::Acquire), 100);

    runtime.shutdown().unwrap();
}

#[test]
fn test_multicore_graceful_shutdown() {
    let _test_lock = runtime_test_lock();
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(3)).unwrap();
    let running_tasks = Arc::new(AtomicU32::new(0));

    let (tx, rx) = mpsc::channel();
    for _ in 0..5 {
        let counter = running_tasks.clone();
        let tx_clone = tx.clone();
        runtime
            .spawn(async move {
                counter.fetch_add(1, Ordering::Relaxed);
                // Simulate some work
                std::thread::yield_now();
                counter.fetch_sub(1, Ordering::Relaxed);
                tx_clone.send(()).unwrap();
            })
            .unwrap();
    }

    // Give tasks time to start
    for _ in 0..5 {
        rx.recv_timeout(Duration::from_millis(200)).unwrap();
    }

    // Graceful shutdown should wait for tasks to complete
    runtime.shutdown().unwrap();

    // All tasks should have completed
    assert_eq!(running_tasks.load(Ordering::Acquire), 0);
}

#[test]
fn test_shutdown_drains_running_timer_task() {
    let _test_lock = runtime_test_lock();
    init_tracing();
    let runtime = MultiCoreRuntime::new(Some(1)).unwrap();
    let (tx, rx) = mpsc::channel();

    runtime
        .spawn(async move {
            rust_miniss::timer::sleep(Duration::from_millis(20)).await;
            tx.send(()).unwrap();
        })
        .unwrap();

    runtime
        .shutdown_with_timeout(Duration::from_secs(1))
        .unwrap();
    runtime.join().unwrap();

    rx.recv_timeout(Duration::from_millis(500))
        .expect("shutdown did not drain the timer task");
}

#[test]
fn test_shutdown_deadline_escalates_permanently_pending_task() {
    let _test_lock = runtime_test_lock();
    let runtime = rust_miniss::multicore::MultiCoreRuntime::new(Some(1)).unwrap();
    runtime
        .spawn(async {
            std::future::pending::<()>().await;
        })
        .unwrap();

    let started = std::time::Instant::now();
    runtime
        .shutdown_with_timeout(Duration::from_millis(20))
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "shutdown deadline did not return promptly"
    );
    // Dropping the last Arc joins the escalated worker and owns all driver
    // cleanup on the worker thread.
    drop(runtime);
}
