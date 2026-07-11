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
use std::io;
use std::os::fd::{AsRawFd as StdAsRawFd, FromRawFd, OwnedFd};
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

/// Shard-local ownership of the descriptor used by an accepted operation.
///
/// The user-facing descriptor may be closed and its number reused while the
/// kernel operation is still queued.  Keeping an independent duplicate alive
/// prevents that reuse from redirecting the in-flight operation.  Lease drop
/// is the terminal reclamation point; dropping `IoFuture` only abandons result
/// delivery.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct DescriptorLease(OwnedFd);

#[allow(dead_code)]
impl DescriptorLease {
    /// Duplicate `fd`, acquiring ownership before an SQE is queued.
    pub(crate) fn acquire(fd: RawFd) -> io::Result<Self> {
        // SAFETY: `dup` returns a fresh descriptor owned by this call on
        // success; `OwnedFd` takes ownership exactly once.
        let duplicate = unsafe { libc::dup(fd) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: duplicate is newly created and exclusively owned here.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(duplicate) }))
    }

    #[inline]
    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(all(test, unix))]
mod descriptor_lease_tests {
    use super::DescriptorLease;

    #[test]
    fn lease_survives_user_fd_close_until_drop() {
        let (read_fd, write_fd) = {
            let mut fds = [0; 2];
            // SAFETY: fds points to two writable RawFd slots.
            let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
            assert_eq!(rc, 0);
            (fds[0], fds[1])
        };
        let lease = DescriptorLease::acquire(read_fd).expect("dup read end");
        // SAFETY: read_fd is still owned by this test and closed exactly once.
        unsafe { libc::close(read_fd) };
        assert_ne!(unsafe { libc::fcntl(lease.as_raw_fd(), libc::F_GETFD) }, -1);
        drop(lease);
        // SAFETY: write_fd is still owned by this test and closed exactly once.
        unsafe { libc::close(write_fd) };
    }
}

/// The core asynchronous I/O backend trait.
///
/// Each CPU thread will own an instance of a type that implements this trait.
/// This design follows the thread-per-core model, avoiding locks within the
/// I/O backend implementation.
pub trait IoProvider: 'static {
    /// The type of completion event returned by the backend.
    type Completion;

    /// Submits an I/O operation to the backend.
    ///
    /// This method is non-blocking. A successful admission returns a unique
    /// `IoToken` to track the operation; rejected operations return an error
    /// without allocating a token or completion slot.
    fn submit(&self, op: Op) -> Result<IoToken, IoError>;

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

    /// Number of accepted operations/results that still require owner-thread
    /// drain before the driver can be reclaimed during graceful shutdown.
    fn pending_operation_count(&self) -> usize {
        0
    }

    /// Park the thread, blocking for completions or timeout.
    /// Returns the number of available completions.
    fn park(&self, _timeout: Duration) -> usize {
        0
    }

    /// Drain and reset park-specific flush metrics.
    ///
    /// Used by the runtime to separate normal event-loop flush cost from
    /// flushes that happen right before blocking in park().
    fn take_park_flush_stats(&self) -> (u64, u64, u64) {
        (0, 0, 0)
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

    /// Fused CQE processing: iterate completions and process them inline
    /// (write to slot + wake task) without an intermediate Vec.
    /// Processes up to `batch_limit` CQEs per call.
    /// Returns `(processed_completions, saw_wakeup_poll_completion, wakeups)`.
    ///
    /// # Safety
    ///
    /// Must be called from the owning CPU thread.
    unsafe fn drain_completions_fused(
        &self,
        _wakeup_poll_token: Option<IoToken>,
        _batch_limit: usize,
    ) -> (usize, bool, usize) {
        (0, false, 0)
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

#[derive(Debug)]
pub enum CompletionKind {
    Accept {
        fd: OwnedFd,
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

#[cfg(all(test, unix))]
mod accept_ownership_tests {
    use super::CompletionKind;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    #[test]
    fn accepted_descriptor_closes_when_completion_is_dropped() {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: pipe returned ownership of this descriptor to the test.
        let accepted = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let raw = accepted.as_raw_fd();
        drop(CompletionKind::Accept {
            fd: accepted,
            addr: None,
        });
        assert_eq!(unsafe { libc::fcntl(raw, libc::F_GETFD) }, -1);
        // SAFETY: the write end remains owned by this test.
        unsafe { libc::close(fds[1]) };
    }
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
