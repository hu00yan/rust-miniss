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
    let runtime = MultiCoreRuntime::with_cpus(num_cpus)?;
    
    println!("✅ Created runtime with {} CPUs", runtime.cpu_count());

    // Demo 1: Basic task distribution
    println!("\n📊 Demo 1: Basic Task Distribution");
    demo_basic_distribution(&runtime)?;

    // Demo 2: CPU-specific tasks
    println!("\n🎯 Demo 2: CPU-Specific Tasks");
    demo_cpu_specific(&runtime)?;

    // Demo 3: Cross-CPU communication
    println!("\n💬 Demo 3: Cross-CPU Communication");
    demo_cross_cpu_communication(&runtime)?;

    // Demo 4: Performance comparison
    println!("\n⚡ Demo 4: Performance Comparison");
    demo_performance_comparison(&runtime)?;

    // Demo 5: Block-on usage
    println!("\n🔄 Demo 5: Block-on Usage");
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

    println!("Spawning {} tasks across {} CPUs...", num_tasks, runtime.cpu_count());
    
    let start = Instant::now();
    
    // Spawn tasks that will be distributed round-robin
    for i in 0..num_tasks {
        let counter_clone = counter.clone();
        runtime.spawn(async move {
            // Simulate some work
            let work_duration = Duration::from_millis(10);
            std::thread::sleep(work_duration);
            
            let current = counter_clone.fetch_add(1, Ordering::SeqCst) + 1;
            println!("  Task {} completed (total: {})", i, current);
        })?;
    }

    // Wait for all tasks to complete
    while counter.load(Ordering::SeqCst) < num_tasks {
        std::thread::sleep(Duration::from_millis(10));
    }
    
    let duration = start.elapsed();
    println!("All {} tasks completed in {:?}", num_tasks, duration);
    
    Ok(())
}

fn demo_cpu_specific(runtime: &MultiCoreRuntime) -> Result<(), Box<dyn std::error::Error>> {
    let results = Arc::new(std::sync::Mutex::new(Vec::new()));
    
    println!("Spawning tasks on specific CPUs...");
    
    // Spawn one task on each CPU
    for cpu_id in 0..runtime.cpu_count() {
        let results_clone = results.clone();
        runtime.spawn_on(cpu_id, async move {
            // Get thread information
            let thread = std::thread::current();
            let thread_name = thread.name().unwrap_or("unknown");
            
            // Simulate CPU-specific work
            std::thread::sleep(Duration::from_millis(50));
            
            let message = format!("CPU {} (thread: {}) completed work", cpu_id, thread_name);
            results_clone.lock().unwrap().push(message);
            
            println!("  ✅ CPU {} finished its work", cpu_id);
        })?;
    }

    // Wait for all CPU-specific tasks to complete
    std::thread::sleep(Duration::from_millis(200));
    
    let final_results = results.lock().unwrap();
    println!("Results from all CPUs:");
    for result in final_results.iter() {
        println!("  • {}", result);
    }
    
    Ok(())
}

fn demo_cross_cpu_communication(runtime: &MultiCoreRuntime) -> Result<(), Box<dyn std::error::Error>> {
    println!("Testing cross-CPU communication with ping...");
    
    // Use the built-in ping functionality
    runtime.ping_all()?;
    
    // Give time for ping messages to be processed
    std::thread::sleep(Duration::from_millis(100));
    
    println!("✅ Ping messages sent between all CPU pairs");
    
    Ok(())
}

fn demo_performance_comparison(runtime: &MultiCoreRuntime) -> Result<(), Box<dyn std::error::Error>> {
    let num_tasks = 100;
    let work_per_task = Duration::from_millis(5);
    
    println!("Comparing multi-core vs single-core performance...");
    println!("Tasks: {}, Work per task: {:?}", num_tasks, work_per_task);
    
    // Multi-core test
    let start = Instant::now();
    let counter = Arc::new(AtomicU32::new(0));
    
    for _ in 0..num_tasks {
        let counter_clone = counter.clone();
        runtime.spawn(async move {
            std::thread::sleep(work_per_task);
            counter_clone.fetch_add(1, Ordering::SeqCst);
        })?;
    }
    
    // Wait for multi-core tasks
    while counter.load(Ordering::SeqCst) < num_tasks {
        std::thread::sleep(Duration::from_millis(1));
    }
    let multi_duration = start.elapsed();
    
    // Single-core test (for comparison)
    let start = Instant::now();
    let single_runtime = Runtime::new();
    let counter2 = Arc::new(AtomicU32::new(0));
    
    for _ in 0..num_tasks {
        let counter_clone = counter2.clone();
        single_runtime.spawn(async move {
            std::thread::sleep(work_per_task);
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });
    }
    
    // Wait for single-core tasks
    while counter2.load(Ordering::SeqCst) < num_tasks {
        std::thread::sleep(Duration::from_millis(1));
    }
    let single_duration = start.elapsed();
    
    println!("Results:");
    println!("  Multi-core ({} CPUs): {:?}", runtime.cpu_count(), multi_duration);
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
    })?;
    
    println!("  Result: 10! = {}", result);
    
    // CPU-specific block_on
    let result = runtime.block_on_cpu(0, async {
        println!("  Running task specifically on CPU 0...");
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("unknown");
        format!("Hello from {}", thread_name)
    })?;
    
    println!("  Message: {}", result);
    
    Ok(())
}
