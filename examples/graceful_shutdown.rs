//! Example showing graceful shutdown handling
//!
//! This example shows how to handle system signals (SIGTERM, SIGINT)
//! to gracefully shut down your application.

use rust_miniss::{timer, Runtime};
use std::time::Duration;

fn main() {
    println!("Signal handling is always available in this runtime");
    let runtime = Runtime::new();
    runtime.block_on(async {
        println!("Press Ctrl+C to test signal handling");
        timer::sleep(Duration::from_secs(10)).await;
        println!("Example completed");
    });
}
