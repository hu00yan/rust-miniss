//! Periodic Task Demo
//!
//! This example demonstrates the use of the `spawn_periodic` utility function
//! to create tasks that execute at regular intervals.

use rust_miniss::task;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Periodic Task Demo");
    println!("=================");

    // Initialize the runtime
    #[cfg(feature = "multicore")]
    {
        rust_miniss::multicore::init_runtime(Some(2))?;
    }

    // Create a counter to track periodic executions
    let counter = Arc::new(AtomicUsize::new(0));

    // Example 1: Simple periodic task
    {
        println!("\n1. Simple periodic task (prints every 500ms)");
        let print_counter = Arc::new(AtomicUsize::new(0));
        let print_counter_clone = print_counter.clone();

        let print_handle = task::spawn_periodic(Duration::from_millis(500), move || {
            let counter = print_counter_clone.clone();
            async move {
                let count = counter.fetch_add(1, Ordering::SeqCst) + 1;
                println!("   Tick #{}", count);
            }
        })?;

        // Let it run for 3 seconds
        std::thread::sleep(Duration::from_secs(3));
        print_handle.cancel()?;
        println!("   Print task cancelled");
    }

    // Example 2: Periodic task with shared state
    {
        println!("\n2. Periodic counter task (increments every 100ms)");
        let counter_clone = counter.clone();

        let counter_handle = task::spawn_periodic(Duration::from_millis(100), move || {
            let counter = counter_clone.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        })?;

        // Let it run for 1 second
        std::thread::sleep(Duration::from_secs(1));
        counter_handle.cancel()?;

        let final_count = counter.load(Ordering::SeqCst);
        println!("   Counter reached: {} (expected ~10)", final_count);
    }

    // Example 3: Multiple periodic tasks with different intervals
    {
        println!("\n3. Multiple periodic tasks with different intervals");

        let fast_counter = Arc::new(AtomicUsize::new(0));
        let slow_counter = Arc::new(AtomicUsize::new(0));

        // Fast task: every 50ms
        let fast_counter_clone = fast_counter.clone();
        let fast_handle = task::spawn_periodic(Duration::from_millis(50), move || {
            let counter = fast_counter_clone.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        })?;

        // Slow task: every 200ms
        let slow_counter_clone = slow_counter.clone();
        let slow_handle = task::spawn_periodic(Duration::from_millis(200), move || {
            let counter = slow_counter_clone.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        })?;

        // Let them run for 1 second
        std::thread::sleep(Duration::from_secs(1));

        // Stop both tasks
        fast_handle.cancel()?;
        slow_handle.cancel()?;

        let fast_count = fast_counter.load(Ordering::SeqCst);
        let slow_count = slow_counter.load(Ordering::SeqCst);

        println!(
            "   Fast task (50ms): {} executions (expected ~20)",
            fast_count
        );
        println!(
            "   Slow task (200ms): {} executions (expected ~5)",
            slow_count
        );
    }

    // Example 4: Periodic task with async work
    {
        println!("\n4. Periodic task performing async work");

        let work_counter = Arc::new(AtomicUsize::new(0));
        let work_counter_clone = work_counter.clone();

        let work_handle = task::spawn_periodic(Duration::from_millis(300), move || {
            let counter = work_counter_clone.clone();
            async move {
                // Simulate some async work
                async_work().await;
                let count = counter.fetch_add(1, Ordering::SeqCst) + 1;
                println!("   Completed async work #{}", count);
            }
        })?;

        // Let it run for 2 seconds
        std::thread::sleep(Duration::from_secs(2));
        work_handle.cancel()?;

        let work_count = work_counter.load(Ordering::SeqCst);
        println!(
            "   Async work completed {} times (expected ~6-7)",
            work_count
        );
    }

    println!("\nAll periodic tasks completed successfully!");
    Ok(())
}

/// Simulates some async work
async fn async_work() {
    // Simulate async computation with a short delay
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(10) {
        // Busy wait to simulate work
        std::hint::spin_loop();
    }
}
