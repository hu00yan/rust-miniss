//! Example demonstrating graceful shutdown via signal handling
//!
//! This example shows how to handle system signals (SIGTERM, SIGINT)
//! to gracefully shut down your application.

#[cfg(feature = "signal")]
use rust_miniss::{timer, Runtime};
#[cfg(feature = "signal")]
use std::time::Duration;

#[cfg(feature = "signal")]
#[tokio::main]
async fn main() {
    #[cfg(feature = "signal")]
    {
        let runtime = Runtime::new();
        runtime.block_on(async {
            // Set up signal handling for graceful shutdown (fallback to Ctrl+C)
            let shutdown_signal = async {
                tokio::signal::ctrl_c()
                    .await
                    .expect("failed to install Ctrl+C handler");
                "SIGINT/Ctrl+C"
            };

            // Your main application logic
            let main_task = async {
                let mut counter = 0;
                loop {
                    timer::sleep(Duration::from_millis(500)).await;
                    counter += 1;
                    println!("Working... iteration {}", counter);

                    // Simulate some work being done
                    if counter >= 20 {
                        println!("Work completed naturally");
                        break;
                    }
                }
            };

            // Wait for either the main task to complete or a shutdown signal
            tokio::select! {
                _ = main_task => {
                    println!("Main task completed successfully");
                }
                sig = shutdown_signal => {
                    println!("Received signal: {:?}, shutting down gracefully...", sig);

                    // Perform cleanup operations
                    println!("Cleaning up resources...");
                    timer::sleep(Duration::from_millis(100)).await;

                    // Close connections, flush data, etc.
                    println!("Cleanup completed, exiting");
                }
            }
        });
    }

}

#[cfg(not(feature = "signal"))]
fn main() {
    println!("Signal handling example requires the 'signal' feature");
    println!("Run with: cargo run --features signal --example graceful_shutdown");
}
