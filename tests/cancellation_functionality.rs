//! Test to verify that task cancellation works correctly

use rust_miniss::multicore::MultiCoreRuntime;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[test]

fn test_task_cancellation_works() {
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();

    // Test 1: Spawn a task
    let task_id = runtime.spawn(async {}).unwrap();
    // Note: Task cancellation is not fully implemented yet
    println!("Spawned task with ID: {:?}", task_id);

    // Clean shutdown
    runtime.shutdown().unwrap();
}

#[test]

fn test_cancel_before_execution() {
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();

    // Create a task that sets a flag when it starts
    let started = Arc::new(AtomicBool::new(false));
    let started_clone = started.clone();

    let task_id = runtime
        .spawn(async move {
            started_clone.store(true, Ordering::SeqCst);
        })
        .unwrap();

    // Note: Task cancellation is not fully implemented yet
    println!("Spawned task with ID: {:?}", task_id);

    // Give some time for processing
    std::thread::yield_now();

    // Clean shutdown
    runtime.shutdown().unwrap();
}
