// Comprehensive demonstration of rust-miniss features
//
// This example showcases:
// - Multi-core runtime usage
// - Timer and timeout functionality  
// - Cross-CPU task distribution
// - Performance measurement

use rust_miniss::{MultiCoreRuntime, timer};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing for better debugging
    tracing_subscriber::fmt::init();

    println!("🚀 Starting Rust-Miniss Comprehensive Demo");
    
    // Create a multi-core runtime with 4 CPUs
    let runtime = MultiCoreRuntime::new(Some(4))?;
    println!("✅ Created multi-core runtime with 4 CPUs");
    
    // Shared counter for demonstrating cross-CPU coordination
    let global_counter = Arc::new(AtomicU32::new(0));
    
    // Feature 1: Timer and Timeout Functionality
    println!("\n⏰ Feature 1: Timer and Timeout Operations");
    runtime.block_on(demonstrate_timer_features())?;
    
    // Feature 2: Simple Async Work
    println!("\n🔄 Feature 2: Simple Async Work");
    runtime.block_on(async {
        println!("  Running async work...");
        timer::sleep(Duration::from_millis(50)).await;
        println!("  ✅ Async work completed!");
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })?;
    
    println!("\n✅ All features demonstrated successfully!");
    println!("📊 Final global counter value: {}", global_counter.load(Ordering::SeqCst));
    
    Ok(())
}

async fn demonstrate_timer_features() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Test 1: Basic sleep
    let start = Instant::now();
    timer::sleep(Duration::from_millis(100)).await;
    let elapsed = start.elapsed();
    println!("  Sleep test completed in {:?}ms", elapsed.as_millis());
    
    // Test 2: Timeout that succeeds
    match timer::timeout(Duration::from_millis(200), async {
        timer::sleep(Duration::from_millis(50)).await;
        "fast_operation"
    }).await {
        Ok(value) => println!("  Timeout success: {}", value),
        Err(_) => println!("  Timeout failed (unexpected)"),
    }
    
    // Test 3: Timeout that expires
    match timer::timeout(Duration::from_millis(50), async {
        timer::sleep(Duration::from_millis(200)).await;
        "slow_operation"
    }).await {
        Ok(value) => println!("  Timeout should have expired, got: {}", value),
        Err(_) => println!("  Timeout expired as expected"),
    }
    
    // Test 4: Using FutureExt timeout combinator
    use rust_miniss::timer::FutureExt;
    
    match async {
        timer::sleep(Duration::from_millis(30)).await;
        "combinator_result"
    }.with_timeout(Duration::from_millis(100)).await {
        Ok(value) => println!("  Combinator timeout: {}", value),
        Err(_) => println!("  Combinator timeout expired"),
    }
    
    Ok(())
}

