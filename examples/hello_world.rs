//! Hello World example for rust-miniss
//! 
//! This example demonstrates the basic usage of our custom async runtime.

use rust_miniss::Runtime;

fn main() {
    println!("🦀 rust-miniss Hello World Example");
    
    let runtime = Runtime::new();
    
    // Example 1: Simple async block
    println!("\n--- Example 1: Simple async block ---");
    let result = runtime.block_on(async {
        println!("Hello from async block!");
        42
    });
    println!("Result: {}", result);
    
    // Example 2: Chained async operations
    println!("\n--- Example 2: Chained operations ---");
    let result = runtime.block_on(async {
        let x = async { 
            println!("Computing first value...");
            10 
        }.await;
        
        let y = async { 
            println!("Computing second value...");
            20 
        }.await;
        
        println!("Adding {} + {} = {}", x, y, x + y);
        x + y
    });
    println!("Final result: {}", result);
    
    // Example 3: Using our custom Future/Promise
    println!("\n--- Example 3: Custom Future/Promise ---");
    let result = runtime.block_on(async {
        let (future, promise) = rust_miniss::future::Future::new();
        
        // Simulate completing the promise from another context
        // In a real scenario, this might be done from a callback or another task
        promise.complete("Hello from Promise!");
        
        future.await
    });
    println!("Promise result: {}", result);
    
    println!("\n✅ All examples completed successfully!");
}
