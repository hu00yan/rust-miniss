//! Simple test to verify basic cancellation functionality

use rust_miniss::multicore::MultiCoreRuntime;

#[test]
fn test_basic_cancellation() {
    let runtime = MultiCoreRuntime::new(Some(2)).unwrap();

    let task_id = runtime.spawn_on(0, async {}).unwrap();
    let cancel_result = runtime.cancel_task(task_id);
    assert!(
        cancel_result.is_ok(),
        "cancel_task on spawned task should succeed"
    );

    let fake_task_id = rust_miniss::waker::TaskId(999999);
    let cancel_result = runtime.cancel_task(fake_task_id);
    assert!(
        cancel_result.is_ok(),
        "cancel_task on non-existent task should still succeed (fire-and-forget)"
    );

    runtime.shutdown().unwrap();
    match std::sync::Arc::try_unwrap(runtime) {
        Ok(rt) => rt.join().unwrap(),
        Err(_) => panic!("Failed to get unique ownership of runtime"),
    }
}
