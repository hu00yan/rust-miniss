//! Unit tests for task cancellation end-to-end functionality
//!
//! Tests covering:
//! 1. Cancel before start
//! 2. Cancel during execution
//! 3. Cancel after completion

use rust_miniss::multicore::MultiCoreRuntime;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::sync::Once;

#[cfg(test)]
mod task_cancellation_tests {
    use super::*;
    use rust_miniss::multicore::MultiCoreRuntime;

    static INIT: Once = Once::new();

    // Sets up the tracing subscriber for tests, ensuring it's only initialized once.
    fn setup_tracing() {
        INIT.call_once(|| {
            tracing_subscriber::fmt::init();
        });
    }

    /// Test canceling a task before it starts execution
    #[test]
    #[cfg(feature = "multicore")]
    fn test_cancel_before_start() {
        setup_tracing();
        let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let started_clone = started.clone();
        let task_id = runtime
            .spawn_on(0, async move {
                started_clone.store(true, Ordering::SeqCst);
            })
            .unwrap();
        runtime.cancel_task(task_id).unwrap();
        // Give some time for the cancellation to be processed
        std::thread::sleep(Duration::from_millis(50));
        assert!(!started.load(Ordering::SeqCst), "Task should not have started after cancellation");
    }

    /// Test canceling a task during execution
    #[test]
    #[cfg(feature = "multicore")]
    fn test_cancel_during_execution() {
        setup_tracing();
        let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let started_clone = started.clone();
        let completed_clone = completed.clone();
        let task_id = runtime
            .spawn_on(1, async move {
                started_clone.store(true, Ordering::SeqCst);
                // Use a short sleep to simulate work
                std::thread::sleep(Duration::from_millis(50));
                completed_clone.store(true, Ordering::SeqCst);
            })
            .unwrap();

        // Wait for task to start
        let mut attempts = 0;
        while !started.load(Ordering::SeqCst) && attempts < 100 {
            std::thread::sleep(Duration::from_millis(5));
            attempts += 1;
        }
        
        runtime.cancel_task(task_id).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(started.load(Ordering::SeqCst), "Task should have started");
        assert!(!completed.load(Ordering::SeqCst), "Task should not have completed due to cancellation");
    }

    /// Test canceling a task after it has already completed
    #[test]
    #[cfg(feature = "multicore")]
    fn test_cancel_after_completion() {
        setup_tracing();
        let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();
        let task_id = runtime
            .spawn_on(0, async move {
                completed_clone.store(true, Ordering::SeqCst);
            })
            .unwrap();

        // Wait for task to complete
        let mut attempts = 0;
        while !completed.load(Ordering::SeqCst) && attempts < 100 {
            std::thread::sleep(Duration::from_millis(5));
            attempts += 1;
        }
        
        std::thread::sleep(Duration::from_millis(50));
        let cancel_result = runtime.cancel_task(task_id);
        assert!(cancel_result.is_err(), "Canceling completed task should return error");
    }

    /// Test that task_cpu_map is properly maintained
    #[test]
    #[cfg(feature = "multicore")]
    fn test_task_cpu_mapping() {
        setup_tracing();
        let runtime = MultiCoreRuntime::new(Some(4)).unwrap();
        let mut task_ids = Vec::new();
        for cpu_id in 0..3 {
            let task_id = runtime.spawn_on(cpu_id, async {}).unwrap();
            task_ids.push(task_id);
        }
        // Give some time for tasks to be processed
        std::thread::sleep(Duration::from_millis(50));
        for task_id in task_ids {
            runtime.cancel_task(task_id).expect("Should be able to cancel tracked task");
        }
    }

    /// Test canceling multiple tasks simultaneously
    #[test]
    #[cfg(feature = "multicore")]
    fn test_cancel_multiple_tasks() {
        setup_tracing();
        let runtime = MultiCoreRuntime::new(Some(4)).unwrap();
        let execution_count = Arc::new(AtomicU32::new(0));
        let mut task_ids = Vec::new();
        for i in 0..10 {
            let execution_count_clone = execution_count.clone();
            let task_id = runtime
                .spawn_on(i % 4, async move {
                    execution_count_clone.fetch_add(1, Ordering::SeqCst);
                })
                .unwrap();
            task_ids.push(task_id);
        }
        std::thread::sleep(Duration::from_millis(50));
        for task_id in task_ids {
            let _ = runtime.cancel_task(task_id);
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    /// Test JoinHandle cancellation interface
    #[test]
    #[cfg(feature = "multicore")]
    fn test_join_handle_cancel() {
        setup_tracing();
        let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let tx_clone = tx.clone();
        let handle = runtime.spawn(async move {
            completed_clone.store(true, Ordering::SeqCst);
            let _ = tx_clone.send(42);
        })
        .unwrap();
        let _ = runtime.cancel_task(handle); // Cancel using the runtime's cancel_task method
        std::thread::sleep(Duration::from_millis(150));
        assert!(!completed.load(Ordering::SeqCst), "Task should not complete after cancellation");
        // Verify that we can't receive a value from the task
        assert!(rx.recv_timeout(Duration::from_millis(10)).is_err());
    }

    /// Test task cancellation with CPU cleanup
    #[test]
    #[cfg(feature = "multicore")]
    fn test_cpu_removes_cancelled_task() {
        setup_tracing();
        let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let started_clone = started.clone();
        let task_id = runtime
            .spawn_on(0, async move {
                started_clone.store(true, Ordering::SeqCst);
            })
            .unwrap();
            
        // Wait for task to start
        let mut attempts = 0;
        while !started.load(Ordering::SeqCst) && attempts < 100 {
            std::thread::sleep(Duration::from_millis(5));
            attempts += 1;
        }
        
        runtime.cancel_task(task_id).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let second_cancel = runtime.cancel_task(task_id);
        assert!(second_cancel.is_err(), "Second cancellation should fail as task is removed");
    }

    /// Test error handling for invalid task IDs
    #[test]
    #[cfg(feature = "multicore")]
    fn test_cancel_invalid_task_id() {
        setup_tracing();
        let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
        let fake_task_id = rust_miniss::waker::TaskId(99999);
        let result = runtime.cancel_task(fake_task_id);
        assert!(result.is_err(), "Canceling non-existent task should return error");
    }

    #[test]
    #[cfg(feature = "multicore")]
    fn test_runtime_lifecycle() {
        setup_tracing();
        let _runtime = MultiCoreRuntime::new(Some(1)).unwrap();
    }
}
