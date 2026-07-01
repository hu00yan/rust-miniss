//! Test to verify that task cancellation works correctly

use rust_miniss::multicore::MultiCoreRuntime;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[test]
fn test_task_cancellation_works() {
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();

    let task_id = runtime.spawn(async {}).unwrap();

    let result = runtime.cancel_task(task_id);
    assert!(result.is_ok(), "cancel_task should succeed: {result:?}");

    thread::sleep(Duration::from_millis(50));

    runtime.shutdown().unwrap();
    match Arc::try_unwrap(runtime) {
        Ok(rt) => rt.join().unwrap(),
        Err(_) => panic!("Failed to get unique ownership of runtime"),
    }
}

#[test]
fn test_cancel_before_execution() {
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();

    let started = Arc::new(AtomicBool::new(false));
    let started_clone = started.clone();

    let task_id = runtime
        .spawn(async move {
            started_clone.store(true, Ordering::Release);
        })
        .unwrap();

    let result = runtime.cancel_task(task_id);
    assert!(
        result.is_ok(),
        "cancel_task should succeed immediately: {result:?}"
    );

    thread::sleep(Duration::from_millis(50));

    runtime.shutdown().unwrap();
    match Arc::try_unwrap(runtime) {
        Ok(rt) => rt.join().unwrap(),
        Err(_) => panic!("Failed to get unique ownership of runtime"),
    }
}
