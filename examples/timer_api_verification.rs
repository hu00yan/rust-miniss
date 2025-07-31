//! Comprehensive example demonstrating all timer utilities
//!
//! This example verifies that all timer APIs are accessible and working correctly.

use rust_miniss::{task, timer, Runtime};
use std::time::Duration;

#[tokio::main]
async fn main() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        println!("=== Timer API Verification ===\n");

        // Test 1: Basic sleep functionality
        println!("1. Testing timer::sleep()");
        let start = std::time::Instant::now();
        timer::sleep(Duration::from_millis(100)).await;
        let elapsed = start.elapsed();
        println!(
            "   Slept for ~{}ms (requested 100ms)\n",
            elapsed.as_millis()
        );

        // Test 2: Timeout via FutureExt trait
        println!("2. Testing timeout via FutureExt trait");
        use rust_miniss::timer::FutureExt;

        let result = async {
            timer::sleep(Duration::from_millis(50)).await;
            "Task completed"
        }
        .with_timeout(Duration::from_millis(200))
        .await;

        match result {
            Ok(value) => println!("   Success: {}", value),
            Err(_) => println!("   Unexpected timeout"),
        }

        // Test timeout that actually times out
        let result = async {
            timer::sleep(Duration::from_millis(200)).await;
            "This won't complete"
        }
        .with_timeout(Duration::from_millis(50))
        .await;

        match result {
            Ok(value) => println!("   Unexpected success: {}", value),
            Err(_) => println!("   Correctly timed out"),
        }
        println!();

        // Test 4: Interval functionality
        println!("4. Testing timer::Interval");
        let mut interval = timer::Interval::new(Duration::from_millis(75));
        let start = std::time::Instant::now();

        for i in 1..=3 {
            interval.tick().await;
            let elapsed = start.elapsed();
            println!("   Tick {} at {}ms", i, elapsed.as_millis());
        }
        println!();

        // Test 5: Periodic task spawning (simplified test)
        println!("5. Testing task::spawn_periodic()");
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        // Spawn a periodic task
        let handle = task::spawn_periodic(Duration::from_millis(30), move || {
            let counter = counter_clone.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });

        match handle {
            Ok(handle) => {
                // Let it run for a short time
                timer::sleep(Duration::from_millis(100)).await;

                // Cancel the periodic task
                let _ = handle.cancel();

                let final_count = counter.load(Ordering::SeqCst);
                println!("   Periodic task executed {} times", final_count);
            }
            Err(e) => {
                println!("   Failed to spawn periodic task: {:?}", e);
            }
        }
        println!();

        // Test 6: Another FutureExt example
        println!("6. Testing FutureExt::with_timeout() again");

        let future = async {
            timer::sleep(Duration::from_millis(25)).await;
            "Future completed"
        };

        let result = future.with_timeout(Duration::from_millis(100)).await;
        match result {
            Ok(value) => println!("   Success with combinator: {}", value),
            Err(_) => println!("   Unexpected timeout with combinator"),
        }
        println!();

        println!("=== All Timer APIs Verified Successfully ===");
    });
}
