//! Hello World example for rust-miniss
//!
//! This example demonstrates the basic usage of our custom async runtime.

use rust_miniss::MultiCoreRuntime;

fn main() {
    println!("rust-miniss Hello World Example");

    let runtime = MultiCoreRuntime::new(Some(1)).unwrap();

    // Example 1: Simple async block
    println!("\n--- Example 1: Simple async block ---");
    let result = runtime.block_on(async {
        println!("Hello from async block!");
        42
    });
    println!("Result: {result}");

    // Example 2: Chained async operations
    println!("\n--- Example 2: Chained operations ---");
    let result = runtime.block_on(async {
        let x = async {
            println!("Computing first value...");
            10
        }
        .await;

        let y = async {
            println!("Computing second value...");
            20
        }
        .await;

        println!("Adding {x} + {y} = {}", x + y);
        x + y
    });
    println!("Final result: {result}");

    // Example 3: Using task spawning (modern approach)
    println!("\n--- Example 3: Task Spawning ---");
    runtime.block_on(async {
        // Spawn a task and await its result
        let handle = runtime.spawn(async {
            println!("Task is running...");
        });

        handle.unwrap();
    });

    println!("\nAll examples completed successfully!");
    runtime.shutdown().unwrap();
}
