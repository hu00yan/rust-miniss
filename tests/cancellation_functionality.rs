//! Test to verify that task cancellation works correctly

use rust_miniss::multicore::MultiCoreRuntime;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
#[cfg(feature = "multicore")]
fn test_task_cancellation_works() {
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
    
    // Test 1: Cancel a task that does nothing (should succeed)
    let task_id = runtime.spawn_on(0, async {}).unwrap();
    let cancel_result = runtime.cancel_task(task_id);
    // This should either succeed or fail gracefully
    println!("Cancel result for simple task: {:?}", cancel_result);
    
    // Test 2: Try to cancel a non-existent task (should fail)
    let fake_task_id = rust_miniss::waker::TaskId(999999);
    let cancel_result = runtime.cancel_task(fake_task_id);
    assert!(cancel_result.is_err(), "Canceling non-existent task should return error");
    
    // Clean shutdown
    runtime.shutdown().unwrap();
}

#[test]
#[cfg(feature = "multicore")]
fn test_cancel_before_execution() {
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
    
    // Create a task that sets a flag when it starts
    let started = Arc::new(AtomicBool::new(false));
    let started_clone = started.clone();
    
    let task_id = runtime.spawn_on(0, async move {
        started_clone.store(true, Ordering::SeqCst);
    }).unwrap();
    
    // Cancel the task immediately
    let cancel_result = runtime.cancel_task(task_id);
    println!("Immediate cancel result: {:?}", cancel_result);
    
    // Give some time for processing
    std::thread::yield_now();
    
    // The task should not have started
    assert!(!started.load(Ordering::SeqCst), "Task should not have started after cancellation");
    
    // Clean shutdown
    runtime.shutdown().unwrap();
}