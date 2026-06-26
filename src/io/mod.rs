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
//! - **Linux with kernel 5.10+ and io_uring feature**: Uses `io_uring` for optimal performance
//! - **Linux (older kernels or without io_uring feature)**: Uses `epoll` via mio
//! - **macOS**: Uses `kqueue` via mio (reuses epoll implementation)
//! - **Other Unix systems**: Uses `epoll` via mio
//!
//! This selection is handled by the build script (`build.rs`) which detects the
//! target platform and kernel version, and sets the appropriate `io_backend`
//! configuration flag.

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use crate::buffer::Buffer;

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
pub trait IoProvider: Send + Sync + 'static {
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

    /// Flush any pending submissions to the kernel.
    /// Called at event loop boundaries to batch-submit SQEs.
    fn flush(&self) {}

    /// Park the thread, blocking for completions or timeout.
    /// Returns the number of available completions.
    fn park(&self, _timeout: Duration) -> usize {
        0
    }

    /// Pop a multi-shot accept result for the given token.
    /// Returns None for non-accept ops or when no result is available.
    fn pop_accept_result(
        &self,
        _token: IoToken,
    ) -> Option<(IoToken, Op, Result<CompletionKind, IoError>)> {
        None
    }
}

/// Represents a specific I/O operation to be performed.
#[derive(Debug, Clone)]
pub enum Op {
    Accept {
        fd: i32,
    },
    Read {
        fd: i32,
        offset: u64,
        len: usize,
    },
    Write {
        fd: i32,
        offset: u64,
        data: Buffer,
    },
    Fsync {
        fd: i32,
    },
    Close {
        fd: i32,
    },
    ReadFile {
        fd: i32,
        offset: u64,
        len: usize,
    },
    WriteFile {
        fd: i32,
        offset: u64,
        data: Buffer,
    },
    UdpRecv {
        fd: RawFd,
        buffer: Buffer,
    },
    UdpSend {
        fd: RawFd,
        data: Buffer,
        addr: SocketAddr,
    },
    UdpRecvVectored {
        fd: RawFd,
        iovecs: Vec<Buffer>,
    },
    UdpSendVectored {
        fd: RawFd,
        iovecs: Vec<Buffer>,
        addr: SocketAddr,
    },
    ReadVectored {
        fd: i32,
        buffers: Vec<Buffer>,
    },
    WriteVectored {
        fd: i32,
        buffers: Vec<Buffer>,
    },
    Connect {
        fd: i32,
        addr: SocketAddr,
    },
    PollAdd {
        fd: i32,
        events: u32,
        multishot: bool,
    },
    /// No-op operation for io_uring throughput benchmarking.
    Nop,
    ReadRent {
        fd: i32,
        buffer: Buffer,
    },
    WriteRent {
        fd: i32,
        data: Buffer,
    },
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
    /// Creates a new, unique `IoToken` from a global atomic counter.
    pub fn new() -> Self {
        Self {
            id: TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Returns the underlying `u64` ID of the token.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Creates an `IoToken` from a raw `u64` ID.
    ///
    /// Used in `io_uring` backend to reconstruct the token from CQE `user_data`.
    /// This is the inverse of `id()`, allowing us to match completions back
    /// to their original submission without any lookup table.
    pub fn from_id(id: u64) -> Self {
        Self { id }
    }
}

/// Describes the result of a successfully completed I/O operation.
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub enum CompletionKind {
    Accept {
        fd: i32,
        addr: Option<SocketAddr>,
    },
    Read {
        bytes_read: usize,
        data: Buffer,
    },
    Write {
        bytes_written: usize,
    },
    ReadVectored {
        bytes_read: usize,
        buffers: Vec<Buffer>,
    },
    WriteVectored {
        bytes_written: usize,
    },
    Fsync,
    Close,
    ReadFile {
        bytes_read: usize,
        data: Buffer,
    },
    WriteFile {
        bytes_written: usize,
    },
    UdpRecv {
        bytes_read: usize,
        buffer: Buffer,
        addr: SocketAddr,
    },
    UdpSend {
        bytes_written: usize,
        data: Buffer,
    },
    UdpRecvVectored {
        bytes_read: usize,
        addr: SocketAddr,
        iovecs: Vec<Buffer>,
    },
    UdpSendVectored {
        bytes_written: usize,
    },
    Connect,
    ReadRent {
        buffer: Buffer,
    },
    WriteRent {
        bytes_written: usize,
        data: Buffer,
    },
    PollAdd {
        events: u32,
    },
    /// A Write on a socket returned -EAGAIN. The io_uring backend has
    /// submitted POLLOUT on the fd and will re-submit the write when
    /// the socket becomes writable. The caller should transfer the waker
    /// and pending_task from the current `token` to `new_token`.
    WriteRetryWaiting {
        new_token: IoToken,
        fd: i32,
    },
    /// No-op completion — used for io_uring NOP benchmarking.
    Nop,
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
            IoError::Other(s) => std::io::Error::other(s),
        }
    }
}

// Conditional compilation for different I/O backend implementations.
// These modules will contain the concrete implementations of the `IoBackend` trait.

#[cfg(io_backend = "kqueue")]
pub mod kqueue;

#[cfg(any(io_backend = "epoll", all(unix, not(target_os = "linux"))))]
pub mod epoll;

#[cfg(all(target_os = "linux", io_backend = "io_uring", feature = "io_uring"))]
pub mod uring;

pub mod future;

#[cfg(any(io_backend = "epoll", all(unix, not(target_os = "linux"))))]
pub use epoll::EpollBackend;
