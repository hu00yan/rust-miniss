//! rust-miniss: A minimal async runtime inspired by Seastar
//! 
//! This crate provides a high-performance async runtime with:
//! - Shared-nothing architecture (one thread per CPU core)
//! - Lock-free cross-CPU communication
//! - io-uring for high-performance I/O on Linux
//! - Custom Future/Promise implementation for educational purposes

#![deny(warnings)]

pub mod future;
pub mod task;
pub mod waker;
pub mod executor;
pub mod cpu;
pub mod multicore;
pub mod config;
pub mod io;
pub mod buffer;

// Re-export core types
pub use future::{Future, Promise};
pub use executor::{Runtime, Executor};
pub use task::{Task, TaskBuilder, TaskError, TaskResult, spawn};
pub use multicore::{MultiCoreRuntime, init_runtime};
pub use cpu::Cpu;
pub use io::{IoBackend, Op, IoToken, CompletionKind, IoError, DummyIoBackend};
pub use buffer::{Buffer, BufferPool};

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
