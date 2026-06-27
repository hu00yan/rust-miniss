//! rust-miniss: A minimal async runtime inspired by Seastar
//!
//! This crate provides a high-performance async runtime with:
//! - Thread-per-core architecture (one thread per CPU core)
//! - Lock-free cross-CPU communication
//! - Automatic IO backend selection (io_uring/kqueue/epoll)
//! - Cooperative signal handling for graceful shutdown
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use rust_miniss::MultiCoreRuntime;
//!
//! let runtime = MultiCoreRuntime::new(Some(4)).unwrap();
//!
//! // Spawn tasks on core threads
//! runtime.spawn(async {
//!     println!("Running on a core thread");
//! }).unwrap();
//!
//! // Block on a future on the current thread
//! let result = runtime.block_on(async { 42 });
//! assert_eq!(result, 42);
//!
//! // Graceful shutdown
//! runtime.shutdown().unwrap();
//! ```
//!
//! # Signal Handling
//!
//! ```rust,no_run
//! use rust_miniss::MultiCoreRuntime;
//!
//! let runtime = MultiCoreRuntime::new(Some(4)).unwrap();
//! runtime.install_signal_handler();
//!
//! // Runtime will shut down gracefully on SIGINT/SIGTERM
//! runtime.block_on(async {
//!     // Your application logic here
//! });
//! ```

#![deny(warnings)]

#[cfg(feature = "arena")]
pub mod arena;
pub mod buffer;
pub mod cancellation;
pub mod compat;
pub mod config;
pub mod cpu;
pub mod executor;
pub mod fs;
pub mod http;
pub mod io;
pub mod multicore;
pub mod net;
pub mod signal;
pub mod task;
pub mod timer;
#[cfg(feature = "tls")]
pub mod tls;
pub mod waker;

pub use buffer::Buffer;
pub use executor::{Executor, Runtime};
pub use fs::AsyncFile;
pub use http::{
    EchoHandler, HttpConnection, HttpHandler, Method, Request, Response, StaticHandler, StatusCode,
};
pub use io::{CompletionKind, IoError, IoProvider, IoToken, Op};
pub use multicore::MultiCoreRuntime;
pub use net::{AsyncTcpListener, AsyncTcpStream, AsyncUdpSocket};
pub use task::{Task, TaskError, TaskResult};
pub use timer::{sleep, timeout, Entry, Interval, TimeoutError, TimerId, TimerWheel};

pub use futures::io::{AsyncRead, AsyncWrite};

pub use compat::{Compat, FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};

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

/// Convenience function to create a runtime and run a future
pub fn block_on<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    let runtime = MultiCoreRuntime::new(Some(1)).expect("Failed to create runtime"); // arch-audit: allow control-plane convenience wrapper
    runtime.block_on(future) // arch-audit: allow control-plane convenience wrapper
}
