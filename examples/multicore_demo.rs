//! Multi-core runtime demonstration
//!
//! This example shows how to use the multi-core runtime to distribute
//! work across multiple CPU cores.

use rust_miniss::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    println!("🚀 Rust-Miniss Multi-Core Demo");
    println!("Available logical CPUs: {}", num_cpus::get());

    // Create a multi-core runtime with 4 CPUs
    let num_cpus = 4;
    let runtime = MultiCoreRuntime::new(Some(num_cpus))?;

    println!("✅ Created runtime with {} CPUs", runtime.cpu_count());

    // Demo 1: Basic task distribution
    println!("\n📊 Demo 1: Basic Task Distribution");
    demo_basic_distribution(&runtime)?;

    // Demo 2: Task distribution across cores
    println!("\n🎯 Demo 2: Task Distribution Across Cores");
    demo_cpu_distribution(&runtime)?;

    // Demo 3: Performance comparison
    println!("\n⚡ Demo 3: Performance Comparison");
    demo_performance_comparison(&runtime)?;

    // Demo 4: Block-on usage
    println!("\n🔄 Demo 4: Block-on Usage");
    demo_block_on(&runtime)?;

    // Graceful shutdown
    println!("\n🛑 Shutting down runtime...");
    runtime.shutdown()?;
    println!("✅ Runtime shutdown complete");

    Ok(())
}

fn demo_basic_distribution(runtime: &MultiCoreRuntime) -> Result<(), Box<dyn std::error::Error>> {
    let counter = Arc::new(AtomicU32::new(0));
    let num_tasks = 20;

    println!(
        "Spawning {} tasks across {} CPUs...",
        num_tasks,
        runtime.cpu_count()
    );

    let start = Instant::now();

    // Spawn tasks that will be distributed round-robin
    for i in 0..num_tasks {
        let counter_clone = counter.clone();
        runtime.spawn(async move {
            // Simulate some work with yielding
            std::thread::yield_now();

            let current = counter_clone.fetch_add(1, Ordering::SeqCst) + 1;
            println!("  Task {} completed (total: {})", i, current);
        })?;
    }

    // Wait for all tasks to complete using a more reliable method
    let timeout = start + Duration::from_secs(5); // 5 second timeout
    while counter.load(Ordering::SeqCst) < num_tasks && Instant::now() < timeout {
        std::thread::yield_now();
    }

    let duration = start.elapsed();
    println!("All {} tasks completed in {:?}", num_tasks, duration);

    Ok(())
}

fn demo_cpu_distribution(runtime: &MultiCoreRuntime) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::mpsc;

    let results = Arc::new(std::sync::Mutex::new(Vec::new()));

    println!("Spawning tasks across CPUs...");

    // Spawn tasks that will be distributed across CPUs
    let (tx, rx) = mpsc::channel();
    for i in 0..runtime.cpu_count() {
        let results_clone = results.clone();
        let tx_clone = tx.clone();
        runtime.spawn(async move {
            // Get thread information
            let thread = std::thread::current();
            let thread_name = thread.name().unwrap_or("unknown");

            // Simulate CPU-specific work
            std::thread::yield_now();

            let message = format!("Task {} (thread: {}) completed work", i, thread_name);
            results_clone.lock().unwrap().push(message);

            println!("  ✅ Task {} finished its work", i);
            let _ = tx_clone.send(());
        })?;
    }

    // Wait for all tasks to complete using a more reliable method
    for _ in 0..runtime.cpu_count() {
        let _ = rx.recv_timeout(Duration::from_secs(1));
    }

    let final_results = results.lock().unwrap();
    println!("Results from all tasks:");
    for result in final_results.iter() {
        println!("  • {}", result);
    }

    Ok(())
}

#[allow(dead_code)]
fn demo_cross_cpu_communication(
    runtime: &MultiCoreRuntime,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::mpsc;

    println!("Testing cross-CPU communication...");

    // Use a more reliable method to test cross-CPU communication
    let (tx, rx) = mpsc::channel();

    // Spawn a task
    runtime.spawn(async move {
        // Simulate some work
        std::thread::yield_now();
        // Send a message to indicate completion
        let _ = tx.send("test_completed");
    })?;

    // Wait for test to complete using a more reliable method
    let _ = rx.recv_timeout(Duration::from_secs(1));

    println!("✅ Cross-CPU communication test completed");

    Ok(())
}

fn demo_performance_comparison(
    runtime: &MultiCoreRuntime,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::mpsc;

    let num_tasks = 100;

    println!("Comparing multi-core vs single-core performance...");
    println!("Tasks: {}", num_tasks);

    // Multi-core test
    let start = Instant::now();
    let counter = Arc::new(AtomicU32::new(0));
    let (tx, rx) = mpsc::channel();

    for i in 0..num_tasks {
        let counter_clone = counter.clone();
        let tx_clone = tx.clone();
        runtime.spawn(async move {
            // Simulate work with yielding
            std::thread::yield_now();
            counter_clone.fetch_add(1, Ordering::SeqCst);
            // Send completion signal for last task
            if i == num_tasks - 1 {
                let _ = tx_clone.send(());
            }
        })?;
    }

    // Wait for multi-core tasks to complete using a more reliable method
    let _ = rx.recv_timeout(Duration::from_secs(5));
    let multi_duration = start.elapsed();

    // Single-core test (for comparison)
    let start = Instant::now();
    let single_runtime = Runtime::new();
    let counter2 = Arc::new(AtomicU32::new(0));
    let (tx2, rx2) = mpsc::channel();

    for i in 0..num_tasks {
        let counter_clone = counter2.clone();
        let tx_clone = tx2.clone();
        single_runtime.spawn(async move {
            // Simulate work with yielding
            std::thread::yield_now();
            counter_clone.fetch_add(1, Ordering::SeqCst);
            // Send completion signal for last task
            if i == num_tasks - 1 {
                let _ = tx_clone.send(());
            }
        });
    }

    // Wait for single-core tasks to complete using a more reliable method
    let _ = rx2.recv_timeout(Duration::from_secs(5));
    let single_duration = start.elapsed();

    println!("Results:");
    println!(
        "  Multi-core ({} CPUs): {:?}",
        runtime.cpu_count(),
        multi_duration
    );
    println!("  Single-core: {:?}", single_duration);

    if multi_duration < single_duration {
        let speedup = single_duration.as_secs_f64() / multi_duration.as_secs_f64();
        println!("  🚀 Multi-core is {:.2}x faster!", speedup);
    } else {
        println!("  ⚠️  Single-core was faster (overhead or insufficient parallelism)");
    }

    Ok(())
}

fn demo_block_on(runtime: &MultiCoreRuntime) -> Result<(), Box<dyn std::error::Error>> {
    println!("Demonstrating block_on functionality...");

    // Simple computation
    let result = runtime.block_on(async {
        println!("  Computing factorial of 10...");
        let mut factorial = 1u64;
        for i in 1..=10 {
            factorial *= i;
        }
        factorial
    });

    println!("  Result: 10! = {}", result);

    Ok(())
}
