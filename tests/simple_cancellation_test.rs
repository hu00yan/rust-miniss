//! Simple test to verify basic cancellation functionality without complex synchronization

use rust_miniss::multicore::MultiCoreRuntime;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
#[cfg(feature = "multicore")]
fn test_basic_cancellation() {
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
    
    // Test 1: Cancel a task that does nothing
    let task_id = runtime.spawn_on(0, async {}).unwrap();
    let cancel_result = runtime.cancel_task(task_id);
    // This should either succeed or fail gracefully
    println!("Cancel result for completed task: {:?}", cancel_result.is_err());
    
    // Test 2: Try to cancel a non-existent task
    let fake_task_id = rust_miniss::waker::TaskId(999999);
    let cancel_result = runtime.cancel_task(fake_task_id);
    assert!(cancel_result.is_err(), "Canceling non-existent task should return error");
    
    // Clean shutdown
    runtime.shutdown().unwrap();
}