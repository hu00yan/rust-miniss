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
//! - **Linux with kernel 6.12+**: Uses `io_uring` for optimal performance
//! - **Linux (older kernels)**: Uses `epoll` via mio
//! - **macOS**: Uses `kqueue` via mio (reuses epoll implementation)
//! - **Other Unix systems**: Uses `epoll` via mio
//!
//! This selection is handled by the build script (`build.rs`) which detects the
//! target platform and kernel version, and sets the appropriate `io_backend`
//! configuration flag.

use std::cell::Cell;
use std::os::unix::io::RawFd;
use std::task::Poll;
use std::time::Duration;

use crate::buffer::Buffer;
use smallvec::SmallVec;

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

    /// Drain completed I/O operations from the backend.
    ///
    /// This method is non-blocking. Returns `Poll::Ready` with completed
    /// operations, or `Poll::Pending` if none are available.
    /// Note: completion-based (io_uring/IOCP), not poll-based (epoll).
    fn drain_completions(&self) -> Poll<Vec<Self::Completion>>;

    /// Drain completed operations into a caller-provided buffer.
    ///
    /// Backends should prefer this path when the caller can reuse allocation.
    /// The default implementation falls back to `drain_completions()`.
    fn drain_completions_into(&self, completions: &mut Vec<Self::Completion>) -> Poll<()> {
        match self.drain_completions() {
            Poll::Ready(mut drained) => {
                completions.append(&mut drained);
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }

    /// Flush any pending submissions to the kernel.
    /// Called at event loop boundaries to batch-submit SQEs.
    fn flush(&self) {}

    /// Number of SQEs currently queued but not yet submitted.
    /// Used by hot loops to tell whether a poll actually produced new work.
    fn pending_submission_count(&self) -> usize {
        0
    }

    /// Park the thread, blocking for completions or timeout.
    /// Returns the number of available completions.
    fn park(&self, _timeout: Duration) -> usize {
        0
    }

    /// Pop a multi-shot accept result for the given token.
    /// Returns None for non-accept ops or when no result is available.
    fn pop_accept_result(&self, _token: IoToken) -> Option<Result<CompletionKind, IoError>> {
        None
    }

    /// Whether Accept operations stay armed after a completion.
    fn supports_multishot_accept(&self) -> bool {
        false
    }

    /// Whether accepted TCP streams are already created in non-blocking mode.
    fn accepted_streams_are_nonblocking(&self) -> bool {
        false
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
    WriteStatic {
        fd: i32,
        data: &'static [u8],
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
        iovecs: SmallVec<[Buffer; 4]>,
    },
    UdpSendVectored {
        fd: RawFd,
        iovecs: SmallVec<[Buffer; 4]>,
        addr: SocketAddr,
    },
    ReadVectored {
        fd: i32,
        buffers: SmallVec<[Buffer; 4]>,
    },
    WriteVectored {
        fd: i32,
        buffers: SmallVec<[Buffer; 4]>,
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

thread_local! {
    static TOKEN_COUNTER: Cell<u64> = const { Cell::new(1) };
}

impl Default for IoToken {
    fn default() -> Self {
        Self::new()
    }
}

impl IoToken {
    /// Creates a new `IoToken` from a thread-local counter.
    ///
    /// Tokens only need to be unique within the owning I/O backend. Keeping
    /// this counter thread-local avoids a cross-core atomic fetch_add on every
    /// I/O submission.
    pub fn new() -> Self {
        TOKEN_COUNTER.with(|counter| {
            let id = counter.get();
            counter.set(id.wrapping_add(1).max(1));
            Self { id }
        })
    }

    /// Creates a new `IoToken` from a caller-owned counter.
    ///
    /// Backends use this to keep token generation shard-local and avoid a
    /// thread-local lookup on every submit.
    #[allow(dead_code)]
    pub(crate) fn next_from(counter: &Cell<u64>) -> Self {
        let id = counter.get();
        counter.set(id.wrapping_add(1).max(1));
        Self { id }
    }

    /// Returns the underlying `u64` ID of the token.
    pub fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn from_id(id: u64) -> Self {
        Self { id }
    }

    /// Returns the shard-local table index encoded in this token.
    ///
    /// io_uring tokens pack a generation in the high 32 bits and the backend
    /// slab slot in the low 32 bits. Other backends currently use small
    /// monotonic IDs, so this is equivalent to `id()` for them.
    #[inline(always)]
    pub(crate) fn slot_index(&self) -> usize {
        (self.id as u32) as usize
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
        buffers: SmallVec<[Buffer; 4]>,
    },
    WriteVectored {
        bytes_written: usize,
        buffers: SmallVec<[Buffer; 4]>,
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
        iovecs: SmallVec<[Buffer; 4]>,
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

#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
pub mod uring;

pub mod future;

#[cfg(any(io_backend = "epoll", all(unix, not(target_os = "linux"))))]
pub use epoll::EpollBackend;
