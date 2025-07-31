use rust_miniss::timer;
use std::time::Duration;

#[tokio::main]
async fn main() {
    println!("Sleeping for 2 seconds...");
    timer::sleep(Duration::from_secs(2)).await;
    println!("Woke up!");

    // Demonstrate timeout functionality
    println!("Testing timeout...");
    let result = timer::timeout(Duration::from_millis(500), async {
        timer::sleep(Duration::from_millis(200)).await;
        "Completed within timeout"
    })
    .await;

    match result {
        Ok(value) => println!("Success: {}", value),
        Err(_) => println!("Operation timed out"),
    }

    // Test timeout that actually times out
    let result = timer::timeout(Duration::from_millis(200), async {
        timer::sleep(Duration::from_millis(500)).await;
        "This won't complete"
    })
    .await;

    match result {
        Ok(value) => println!("Success: {}", value),
        Err(_) => println!("Operation timed out (as expected)"),
    }
}
