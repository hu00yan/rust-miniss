//! rust-miniss: A minimal async runtime inspired by Seastar
//!
//! This crate provides a high-performance async runtime with:
//! - Shared-nothing architecture (one thread per CPU core)
//! - Lock-free cross-CPU communication
//! - Automatic IO backend selection for optimal performance
//! - Custom Future/Promise implementation for educational purposes
//!
//! ## IO Backend Selection
//!
//! The runtime automatically selects the most appropriate IO backend for your platform:
//!
//! - **Linux with kernel 5.10+**: Uses `io_uring` for optimal performance
//! - **macOS**: Uses `kqueue`
//! - **Other Unix systems**: Uses `epoll`
//!
//! This selection happens at compile time based on your target platform and kernel version.
//! See `build.rs` for the complete selection logic.
//!
//! # Timer Utilities
//!
//! The runtime provides several timer utilities for async timing operations:
//!
//! - [`timer::sleep()`] - Asynchronously wait for a duration
//! - [`timer::timeout()`] - Apply a timeout to any future
//! - [`timer::Interval`] - Create repeating timers for periodic tasks
//!
//! ## Examples
//!
//! ```rust,no_run
//! use rust_miniss::timer;
//! use std::time::Duration;
//!
//! # async fn example() {
//! // Sleep for 1 second
//! timer::sleep(Duration::from_secs(1)).await;
//!
//! // Apply a timeout to an operation
//! let result = timer::timeout(Duration::from_secs(5), async {
//!     // Some long-running operation
//!     timer::sleep(Duration::from_secs(2)).await;
//!     "completed"
//! }).await;
//!
//! // Create a periodic interval
//! let mut interval = timer::Interval::new(Duration::from_millis(100));
//! for _ in 0..5 {
//!     interval.tick().await;
//!     println!("tick");
//! }
//! # }
//! ```
//!
//! # Graceful Shutdown via Signals
//!
//! The runtime supports graceful shutdown through signal handling. When enabled
//! with the `signal` feature, the runtime can listen for termination signals
//! (SIGTERM, SIGINT) and initiate a graceful shutdown sequence.
//!
//! ## Signal Handling Example
//!
//! ```rust,ignore
//! use rust_miniss::{Runtime, timer, signal::SignalHandler};
//! use std::sync::atomic::{AtomicBool, Ordering};
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! let runtime = Runtime::new();
//! let shutdown_flag = Arc::new(AtomicBool::new(false));
//!
//! // Set up signal handling for graceful shutdown
//! #[cfg(feature = "signal")]
//! {
//!     let handler = SignalHandler::new(shutdown_flag.clone());
//!     handler.start();
//! }
//!
//! runtime.block_on(async {
//!     // Your main application logic
//!     loop {
//!         if shutdown_flag.load(Ordering::SeqCst) {
//!             println!("Received shutdown signal, exiting gracefully...");
//!             // Perform cleanup operations here
//!             timer::sleep(Duration::from_millis(100)).await;
//!             break;
//!         }
//!         
//!         // Do some work
//!         timer::sleep(Duration::from_millis(100)).await;
//!         println!("Working...");
//!     }
//! });
//! ```

#![deny(warnings)]

pub mod buffer;
pub mod cancellation;
pub mod config;
pub mod cpu;
pub mod executor;
pub mod future;
pub mod io;
pub mod net;
pub mod multicore;
pub mod task;
pub mod timer;
pub mod waker;

#[cfg(feature = "signal")]
pub mod signal;

// Re-export core types
pub use buffer::{Buffer, BufferPool};
pub use cpu::Cpu;
pub use executor::{Executor, Runtime};
pub use future::{Future, Promise};
pub use io::{CompletionKind, DummyIoBackend, IoBackend, IoError, IoToken, Op};
pub use multicore::{init_runtime, MultiCoreRuntime};
pub use task::{spawn, Task, TaskBuilder, TaskError, TaskResult};
pub use timer::{sleep, timeout, Entry, Interval, TimeoutError, TimerId, TimerWheel};

/// Error types for the runtime
pub mod error {
    use thiserror::Error;

    #[derive(Error, Debug)]
    pub enum RuntimeError {
        #[error("Runtime not initialized")]
        NotInitialized,

        #[error("Task execution failed: {0}")]
        TaskFailed(String),

        #[error("IO operation failed: {0}")]
        IoFailed(#[from] std::io::Error),
    }

    pub type Result<T> = std::result::Result<T, RuntimeError>;
}

/// Convenience function to create a new runtime and run a future
pub fn block_on<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    let runtime = Runtime::new();
    runtime.block_on(future)
}
