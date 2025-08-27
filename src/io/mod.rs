//! Core I/O abstractions for the runtime.
//!
//! This module defines the `IoBackend` trait, which provides a generic interface
//! for different asynchronous I/O mechanisms like `io-uring`, `epoll`, or `kqueue`.
//! Each CPU thread will own an instance of an `IoBackend` implementation.
//!
//! ## IO Backend Selection
//!
//! The IO backend is automatically selected at compile time based on the target platform
//! and kernel version:
//!
//! - **Linux with kernel 5.10+**: Uses `io_uring` for optimal performance
//! - **Linux with older kernels**: Falls back to a dummy backend
//! - **macOS**: Uses `kqueue`
//! - **Other Unix systems**: Uses `epoll`
//!
//! This selection is handled by the build script (`build.rs`) which detects the
//! target platform and kernel version, and sets the appropriate `io_backend` 
//! configuration flag.

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

/// A trait for I/O resources that can be represented by a raw file descriptor.
pub trait AsRawFd {
    /// Returns the raw file descriptor of this I/O resource.
    fn as_raw_fd(&self) -> RawFd;
}

/// The core asynchronous I/O backend trait.
///
/// Each CPU thread will own an instance of a type that implements this trait.
/// This design follows the thread-per-core model, avoiding locks within the
/// I/O backend implementation.
pub trait IoBackend: Send + Sync + 'static {
    /// The type of completion event returned by the backend.
    type Completion;

    /// Submits an I/O operation to the backend.
    ///
    /// This method is non-blocking and returns a unique `IoToken` to track
    /// the operation.
    fn submit(&self, op: Op) -> IoToken;

    /// Polls for completed I/O operations.
    ///
    /// This method is non-blocking. If no completions are ready, it may
    /// register the waker to be notified when completions are available.
    fn poll_complete(&self, cx: &mut Context<'_>) -> Poll<Vec<Self::Completion>>;
}

/// Represents a specific I/O operation to be performed.
#[derive(Debug, Clone)]
pub enum Op {
    Accept { fd: i32 },
    Read { fd: i32, offset: u64, len: usize },
    Write { fd: i32, offset: u64, data: Vec<u8> },
    Fsync { fd: i32 },
    Close { fd: i32 },
    ReadFile { fd: i32, offset: u64, len: usize },
    WriteFile { fd: i32, offset: u64, data: Vec<u8> },
}

/// A unique identifier for a submitted I/O operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IoToken {
    id: u64,
}

static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(1);

impl Default for IoToken {
    fn default() -> Self {
        Self::new()
    }
}

impl IoToken {
    /// Creates a new, unique `IoToken`.
    pub fn new() -> Self {
        Self {
            id: TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Returns the underlying `u64` ID of the token.
    pub fn id(&self) -> u64 {
        self.id
    }
}

/// Describes the result of a successfully completed I/O operation.
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub enum CompletionKind {
    Accept { fd: i32, addr: Option<SocketAddr> },
    Read { bytes_read: usize, data: Vec<u8> },
    Write { bytes_written: usize },
    Fsync,
    Close,
    ReadFile { bytes_read: usize, data: Vec<u8> },
    WriteFile { bytes_written: usize },
}

/// Represents an error that can occur during an I/O operation.
#[derive(Debug)]
pub enum IoError {
    /// An error originating from the underlying `std::io` module.
    Io(std::io::Error),
    /// Another type of error, represented as a string.
    Other(String),
}

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IoError::Io(err) => write!(f, "IO error: {err}"),
            IoError::Other(msg) => write!(f, "Other error: {msg}"),
        }
    }
}

impl std::error::Error for IoError {}

impl From<IoError> for std::io::Error {
    fn from(e: IoError) -> Self {
        match e {
            IoError::Io(err) => err,
            IoError::Other(s) => std::io::Error::new(std::io::ErrorKind::Other, s),
        }
    }
}

// Conditional compilation for different I/O backend implementations.
// These modules will contain the concrete implementations of the `IoBackend` trait.

#[cfg(any(target_os = "macos", io_backend = "kqueue"))]
pub mod kqueue;

#[cfg(any(
    all(unix, not(target_os = "macos")),
    all(target_os = "linux", io_backend = "epoll")
))]
pub mod epoll;

#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
pub mod uring;

pub mod future;


// --- Dummy Backend for testing and fallback ---

/// A no-op I/O backend that does nothing.
/// Useful for testing the runtime without actual I/O, or as a fallback.
#[derive(Debug, Default)]
pub struct DummyIoBackend;

impl DummyIoBackend {
    pub fn new() -> Self {
        Self
    }
}

impl IoBackend for DummyIoBackend {
    type Completion = (IoToken, Op, std::result::Result<CompletionKind, IoError>);

    fn submit(&self, _op: Op) -> IoToken {
        // Return a new token, but the operation will never complete.
        IoToken::new()
    }

    fn poll_complete(&self, _cx: &mut Context<'_>) -> Poll<Vec<Self::Completion>> {
        // The dummy backend never has completions, so it always returns an empty vector.
        Poll::Ready(Vec::new())
    }
}
