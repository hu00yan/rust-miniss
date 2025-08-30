//! TaskBuilder demonstration
//!
//! This example shows how to use the TaskBuilder to spawn tasks
//! with automatic runtime backend selection.

use rust_miniss::{spawn, TaskBuilder};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("TaskBuilder Demo");
    println!("================");

    // Demo 1: Using TaskBuilder directly
    println!("\n1. Using TaskBuilder directly:");
    let builder = TaskBuilder::new();
    let handle = builder.spawn(async {
        println!("Task executed via TaskBuilder!");
        42
    })?;

    // Use tokio runtime to await the handle
    let result = handle.await;
    println!("Task result: {:?}", result);

    // Demo 2: Using the convenience spawn function
    println!("\n2. Using convenience spawn function:");
    let counter = Arc::new(AtomicU32::new(0));

    // Spawn multiple tasks
    let mut handles = Vec::new();
    for i in 0..5 {
        let counter_clone = counter.clone();
        let handle = spawn(async move {
            counter_clone.fetch_add(i, Ordering::SeqCst);
            format!("Task {} completed", i)
        })?;
        handles.push(handle);
    }

    // Wait for all tasks to complete
    for (i, handle) in handles.into_iter().enumerate() {
        let result = handle.await;
        println!("Handle {}: {:?}", i, result);
    }

    // Give background threads some time to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    println!("Final counter value: {}", counter.load(Ordering::SeqCst));

    println!("\n3. TaskBuilder automatic backend selection:");
    println!("   - Without multicore feature: Uses single-CPU executor");

    println!("   - With multicore feature: Uses multi-CPU runtime if initialized");

    println!("   - Multicore feature not enabled");

    Ok(())
}
