//! Core I/O abstractions for the runtime.
//!
//! This module defines the `IoBackend` trait, which provides a generic interface
//! for different asynchronous I/O mechanisms like `io-uring` or a
//! `mio`-based readiness backend.
//! Each CPU thread will own an instance of an `IoBackend` implementation.
//!
//! ## IO Backend Selection
//!
//! The IO backend is automatically selected at compile time based on the target platform
//! and kernel version:
//!
//! - **Linux with kernel 6.12+**: Uses `io_uring` for optimal performance
//! - **Linux (older kernels)**: Uses the `mio` readiness backend (`epoll` underneath)
//! - **macOS**: Uses the same `mio` readiness backend (`kqueue` underneath).
//!   `mio` normalizes readiness notification, but each target still needs its
//!   own correctness validation. `MioBackend::park` blocks through
//!   `mio::Poll::poll` on both epoll and kqueue. Mio also implements the
//!   `Op::ReadRent`/`Op::WriteRent` owned-buffer paths. File flushes (`Op::Fsync`,
//!   `Op::Fdatasync`) and file reads and writes run on the backend's bounded
//!   thread pool, never on the runtime thread; flushes
//!   requested by tasks in the same shard round are group-committed per inode
//!   (see `crate::io::mio_poll`). Other Unix targets are not supported or tested.
//!
//! This selection is handled by `build.rs` at compile time. Its Linux `uname`
//! probe reads the build machine's kernel and assumes build and runtime machines
//! match; cross-compiling callers should set `MINISS_IO_BACKEND` for the target.

use std::cell::Cell;
use std::io;
use std::os::fd::{AsRawFd as StdAsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use crate::buffer::Buffer;
use smallvec::SmallVec;

/// Kernel listen backlog and bounded multishot-accept delivery capacity.
/// Keeping these equal prevents an accepted burst from being reset merely
/// because user-space delivery is smaller than the advertised backlog.
pub(crate) const ACCEPT_BACKLOG_CAPACITY: usize = 1024;

/// Synthetic termination marker for a multishot receive arm.
///
/// The kernel terminates a multishot arm either with a zero result (peer
/// close — surfaced as an empty `CompletionKind::RecvMultishot` payload) or
/// with an errno (ring exhausted `-ENOBUFS`, cancellation `-ECANCELED`, or a
/// socket error — surfaced as `IoError::Io`). The one kernel shape that fits
/// neither is a final data CQE (positive result, no `IORING_CQE_F_MORE`);
/// after its data is delivered, the backend queues this marker so the net
/// layer knows the arm is dead and re-arms on the next read. It never
/// escapes `AsyncTcpStream::read_rent`.
pub(crate) const MULTISHOT_ARM_TERMINATED: &str = "miniss multishot recv arm terminated";

/// Provided-buffer-ring group id shared by the io_uring backend and the net
/// layer (`Op::RecvMultishot`). Declared here — not in the Linux-only
/// `buf_ring` module — so the net layer can name it on every platform.
pub(crate) const MULTISHOT_BUF_GROUP: u16 = 1;

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
/// Shared socket-classification cell for one registered descriptor.
///
/// `0` = unprobed, `1` = socket, `2` = not a socket. Every lease of a
/// registered descriptor shares one cell, so the kernel only has to be asked
/// once per descriptor lifetime. The cell dies with the descriptor's `Arc`,
/// so a cached class can never outlive the fd it was probed on and cannot
/// alias a reused descriptor number.
pub(crate) const DESCRIPTOR_CLASS_UNKNOWN: u8 = 0;
pub(crate) const DESCRIPTOR_CLASS_SOCKET: u8 = 1;
pub(crate) const DESCRIPTOR_CLASS_NOT_SOCKET: u8 = 2;

pub(crate) struct RegisteredDescriptor {
    fd: Arc<OwnedFd>,
    class: Arc<std::sync::atomic::AtomicU8>,
}

impl std::fmt::Debug for RegisteredDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredDescriptor")
            .field("fd", &self.fd.as_raw_fd())
            .finish_non_exhaustive()
    }
}

#[allow(dead_code)]
#[derive(Debug)]
#[doc(hidden)]
pub struct DescriptorLease {
    fd: Arc<OwnedFd>,
    class: Arc<std::sync::atomic::AtomicU8>,
}

#[allow(dead_code)]
impl RegisteredDescriptor {
    /// Transfer an already-owned descriptor into operation-safe ownership.
    pub(crate) fn from_owned(owned: OwnedFd) -> Self {
        Self {
            fd: Arc::new(owned),
            class: Arc::new(std::sync::atomic::AtomicU8::new(DESCRIPTOR_CLASS_UNKNOWN)),
        }
    }

    /// Import one descriptor identity into shard-local ownership.
    pub(crate) fn acquire(fd: RawFd) -> io::Result<Self> {
        // SAFETY: dup returns a fresh descriptor owned by this call on
        // success; OwnedFd takes ownership exactly once.
        let duplicate = unsafe { libc::dup(fd) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: duplicate is newly created and exclusively owned here.
        let owned = unsafe { OwnedFd::from_raw_fd(duplicate) };
        Ok(Self::from_owned(owned))
    }

    /// Create one operation-local lease without another descriptor syscall.
    #[inline]
    pub(crate) fn lease(&self) -> DescriptorLease {
        DescriptorLease {
            fd: Arc::clone(&self.fd),
            class: Arc::clone(&self.class),
        }
    }

    #[inline]
    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Transfer the descriptor to a raw-fd caller. If a kernel operation still
    /// owns a lease, return a duplicate and let the original live to terminal
    /// completion; otherwise transfer the original identity without a syscall.
    pub(crate) fn into_raw_fd(self) -> io::Result<RawFd> {
        match Arc::try_unwrap(self.fd) {
            Ok(owned) => Ok(owned.into_raw_fd()),
            Err(shared) => {
                // SAFETY: dup creates a new descriptor owned by the caller.
                let duplicate = unsafe { libc::dup(shared.as_raw_fd()) };
                if duplicate < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(duplicate)
                }
            }
        }
    }
}

#[allow(dead_code)]
impl DescriptorLease {
    /// Duplicate `fd`, acquiring ownership before an SQE is queued.
    pub(crate) fn acquire(fd: RawFd) -> io::Result<Self> {
        RegisteredDescriptor::acquire(fd).map(|registered| registered.lease())
    }

    /// Create another operation-local lease without duplicating the descriptor.
    #[inline]
    pub(crate) fn lease(&self) -> Self {
        Self {
            fd: Arc::clone(&self.fd),
            class: Arc::clone(&self.class),
        }
    }

    #[inline]
    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Cached socket classification for this descriptor identity, if probed.
    /// Only the owner thread probes and stores; relaxed ordering is enough
    /// because a stale "unprobed" answer merely repeats the probe.
    pub(crate) fn socket_class(&self) -> Option<bool> {
        match self.class.load(std::sync::atomic::Ordering::Relaxed) {
            DESCRIPTOR_CLASS_SOCKET => Some(true),
            DESCRIPTOR_CLASS_NOT_SOCKET => Some(false),
            _ => None,
        }
    }

    /// Publish a probe result to every lease of this descriptor.
    pub(crate) fn store_socket_class(&self, is_socket: bool) {
        let class = if is_socket {
            DESCRIPTOR_CLASS_SOCKET
        } else {
            DESCRIPTOR_CLASS_NOT_SOCKET
        };
        self.class
            .store(class, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(all(test, unix))]
mod descriptor_lease_tests {
    use super::{DescriptorLease, RegisteredDescriptor};
    use std::os::fd::{FromRawFd, OwnedFd};

    static_assertions::assert_impl_all!(RegisteredDescriptor: Send, Sync);
    static_assertions::assert_impl_all!(DescriptorLease: Send, Sync);
    static_assertions::assert_not_impl_any!(DescriptorLease: Clone);

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
        // SAFETY: the lease holds a live duplicated descriptor; `F_GETFD` only queries it.
        assert_ne!(unsafe { libc::fcntl(lease.as_raw_fd(), libc::F_GETFD) }, -1);
        drop(lease);
        // SAFETY: write_fd is still owned by this test and closed exactly once.
        unsafe { libc::close(write_fd) };
    }

    #[test]
    fn leases_of_one_descriptor_share_the_socket_classification() {
        let (read_fd, write_fd) = {
            let mut fds = [0; 2];
            // SAFETY: fds points to two writable RawFd slots.
            let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
            assert_eq!(rc, 0);
            (fds[0], fds[1])
        };
        let registered = RegisteredDescriptor::acquire(read_fd).expect("register read end");
        // A freshly duplicated descriptor starts unclassified: a raw-fd lease
        // must not inherit the class recorded for a different descriptor that
        // once held the same number.
        let fresh = DescriptorLease::acquire(read_fd).expect("dup read end");
        // SAFETY: read_fd is still owned by this test and closed exactly once.
        unsafe { libc::close(read_fd) };

        let first = registered.lease();
        let second = registered.lease();
        assert_eq!(first.socket_class(), None);
        first.store_socket_class(true);
        assert_eq!(second.socket_class(), Some(true));

        assert_eq!(fresh.socket_class(), None);
        fresh.store_socket_class(false);
        assert_eq!(fresh.socket_class(), Some(false));
        assert_eq!(first.socket_class(), Some(true));

        drop(registered);
        drop(first);
        drop(second);
        drop(fresh);
        // SAFETY: write_fd is still owned by this test and closed exactly once.
        unsafe { libc::close(write_fd) };
    }

    #[test]
    fn registered_descriptor_shares_one_owned_identity_until_last_operation_lease() {
        let (read_fd, write_fd) = {
            let mut fds = [0; 2];
            // SAFETY: fds points to two writable RawFd slots.
            let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
            assert_eq!(rc, 0);
            (fds[0], fds[1])
        };
        let registered = RegisteredDescriptor::acquire(read_fd).expect("register read end");
        // SAFETY: read_fd is still owned by this test and closed exactly once.
        unsafe { libc::close(read_fd) };

        let first = registered.lease();
        let second = registered.lease();
        let leased_fd = first.as_raw_fd();
        assert_eq!(leased_fd, second.as_raw_fd());
        // SAFETY: `libc::stat` is plain C storage; the `fstat` below fills it entirely.
        let mut before_drop = unsafe { std::mem::zeroed::<libc::stat>() };
        // SAFETY: unique `&mut before_drop`, and the lease keeps `leased_fd` alive.
        assert_eq!(unsafe { libc::fstat(leased_fd, &mut before_drop) }, 0);

        drop(registered);
        drop(first);
        // SAFETY: `second` still holds a lease, so `leased_fd` is live; `F_GETFD` only queries it.
        assert_ne!(unsafe { libc::fcntl(leased_fd, libc::F_GETFD) }, -1);

        drop(second);
        // Another test may reuse the numeric fd immediately after the last
        // lease closes it. Distinguish that valid reuse from an unexpectedly
        // retained descriptor instead of asserting on the number alone.
        // SAFETY: `libc::stat` is plain C storage; the `fstat` below fills it entirely.
        let mut after_drop = unsafe { std::mem::zeroed::<libc::stat>() };
        // SAFETY: `fstat` on any raw fd value is memory-safe; `after_drop` is a live local.
        let rc = unsafe { libc::fstat(leased_fd, &mut after_drop) };
        if rc == 0 {
            assert_ne!(
                (before_drop.st_dev, before_drop.st_ino),
                (after_drop.st_dev, after_drop.st_ino),
                "last lease still retained the original descriptor"
            );
        } else {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EBADF)
            );
        }
        // SAFETY: write_fd is still owned by this test and closed exactly once.
        unsafe { libc::close(write_fd) };
    }

    #[test]
    fn owned_registration_keeps_original_fd_and_duplicates_only_for_shared_transfer() {
        let mut fds = [0; 2];
        // SAFETY: fds points to two writable RawFd slots.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: ownership of the read end is transferred exactly once.
        let owned = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let registered = RegisteredDescriptor::from_owned(owned);
        assert_eq!(registered.as_raw_fd(), fds[0]);

        let lease = registered.lease();
        let transferred = registered
            .into_raw_fd()
            .expect("shared transfer duplicates for caller ownership");
        assert_ne!(transferred, lease.as_raw_fd());
        // SAFETY: the lease is live and `F_GETFD` only queries the descriptor.
        assert!(unsafe { libc::fcntl(lease.as_raw_fd(), libc::F_GETFD) } >= 0);

        drop(lease);
        // SAFETY: transferred and the pipe write end remain test-owned.
        unsafe {
            libc::close(transferred);
            libc::close(fds[1]);
        }
    }
}

/// Owner-published acquire/release balance for one shard's I/O resources.
///
/// Published into [`crate::multicore::CoreTerminationSnapshot`] so shutdown
/// accounting is evidence-bearing: at quiescence every acquire must have a
/// matching release (`*_acquired == *_released`) and nothing may remain
/// in flight (`*_held == *_live == 0`). Mid-stream, `acquired − released`
/// is the legitimate in-flight set; it is asserted, not zeroed, at every
/// drain/park boundary in debug builds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceBalanceSnapshot {
    /// Lease installs counted on this shard.
    pub descriptor_leases_acquired: u64,
    /// Terminal lease reclaims counted on this shard.
    pub descriptor_leases_released: u64,
    /// Leases still held in the backend lease table at publication time.
    pub descriptor_leases_held: u64,
    /// Operations admitted into the backend pending set.
    pub pending_ops_acquired: u64,
    /// Operations removed from the pending set (terminal CQE, cancel,
    /// submission failure).
    pub pending_ops_released: u64,
    /// Operations still pending in the backend at publication time.
    pub pending_ops_live: u64,
}

impl ResourceBalanceSnapshot {
    /// Quiescence form of the balance equation: every acquire is paired with
    /// exactly one release and nothing is in flight.
    pub fn is_balanced_and_empty(&self) -> bool {
        self.descriptor_leases_acquired == self.descriptor_leases_released
            && self.pending_ops_acquired == self.pending_ops_released
            && self.descriptor_leases_held == 0
            && self.pending_ops_live == 0
    }
}

/// Owner-thread acquire/release counters for the resources whose pairing the
/// forensics review (docs/review/defect-forensics-2026-10.md, P3) identified
/// as structurally error-prone: descriptor leases and pending operations.
///
/// These are plain `Cell<u64>` fields mutated only by the backend's owning
/// thread — the same discipline as `CoreMetrics` and MioBackend's flush
/// counters (AGENTS.md MUST #3/#6). No atomics, no locks, no cross-core
/// traffic: the only reader outside the owner thread is the termination
/// snapshot, which is collected after the owner exits.
///
/// # The balance equations
///
/// For each resource, with `A` = acquired, `R` = released, `L` = live
/// (counted by scanning the backend's own table — the ground truth, not a
/// third counter):
///
/// ```text
/// R == A − L        asserted at every drain/park boundary (debug builds)
/// R == A ∧ L == 0   asserted at quiescence via ResourceBalanceSnapshot
/// ```
///
/// The equation is *not* "always zero": in-flight operations are legitimate.
/// Counting mutations only at table transitions means the equation detects
/// *pairing* drift — a release path that clears the table without counting,
/// a counting path that clears nothing, or a slot taken from the pending set
/// and neither reinstated nor recycled (the op is lost; `L` drops below
/// `A − R` and the assert fires). It deliberately does *not* detect a
/// resource that is never released at all: an abandoned entry stays in the
/// table, so the equation still balances while `L > 0`. That class is caught
/// by the quiescence half at shutdown, where every legitimate in-flight set
/// is empty and the snapshot must be `is_balanced_and_empty()`.
///
/// Backend in-flight models:
/// - descriptor leases: an entry in the backend's lease table (installed at
///   submit/registration, removed at terminal reclaim).
/// - pending ops: an entry in the backend's pending-operation set (slab for
///   io_uring, slot table for mio). Multishot accepts stay one pending op
///   while armed; their queued-but-unconsumed results are accounted by the
///   existing `pending_operation_count`, not by this ledger.
#[derive(Debug, Default)]
pub(crate) struct ResourceLedger {
    leases_acquired: Cell<u64>,
    leases_released: Cell<u64>,
    ops_acquired: Cell<u64>,
    ops_released: Cell<u64>,
}

impl ResourceLedger {
    /// Count one lease installed into the backend lease table.
    #[inline]
    pub(crate) fn lease_acquired(&self) {
        self.leases_acquired.set(self.leases_acquired.get() + 1);
    }

    /// Count one lease removed from the backend lease table.
    #[inline]
    pub(crate) fn lease_released(&self) {
        self.leases_released.set(self.leases_released.get() + 1);
    }

    /// Count one operation admitted into the backend pending set.
    #[inline]
    pub(crate) fn op_acquired(&self) {
        self.ops_acquired.set(self.ops_acquired.get() + 1);
    }

    /// Count one operation removed from the backend pending set.
    #[inline]
    pub(crate) fn op_released(&self) {
        self.ops_released.set(self.ops_released.get() + 1);
    }

    /// Assert both balance equations against the backend's live table counts.
    ///
    /// Debug-build only and free in release: `cfg!(debug_assertions)` is a
    /// compile-time constant, so callers pass live counts produced by
    /// table scans inside the same gated block and the scans are eliminated
    /// entirely from release builds.
    #[inline]
    pub(crate) fn assert_balanced(
        &self,
        live_leases: usize,
        live_ops: usize,
        context: &'static str,
    ) {
        if cfg!(debug_assertions) {
            let (acquired, released) = (self.leases_acquired.get(), self.leases_released.get());
            debug_assert!(
                acquired as i128 - released as i128 == live_leases as i128,
                "descriptor lease ledger imbalance at {context}: acquired={acquired} released={released} in_flight={live_leases} (expected released == acquired - in_flight)"
            );
            let (acquired, released) = (self.ops_acquired.get(), self.ops_released.get());
            debug_assert!(
                acquired as i128 - released as i128 == live_ops as i128,
                "pending op ledger imbalance at {context}: acquired={acquired} released={released} in_flight={live_ops} (expected released == acquired - in_flight)"
            );
        }
    }

    /// Owner-published balance snapshot for termination reporting.
    /// `live_*` come from the same owner-thread table scans as the asserts.
    #[inline]
    pub(crate) fn snapshot(&self, live_leases: usize, live_ops: usize) -> ResourceBalanceSnapshot {
        ResourceBalanceSnapshot {
            descriptor_leases_acquired: self.leases_acquired.get(),
            descriptor_leases_released: self.leases_released.get(),
            descriptor_leases_held: live_leases as u64,
            pending_ops_acquired: self.ops_acquired.get(),
            pending_ops_released: self.ops_released.get(),
            pending_ops_live: live_ops as u64,
        }
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

    fn uses_completion_io(&self) -> bool {
        false
    }

    /// Submit using descriptor ownership already imported by a shard-local
    /// resource. Backends that do not specialize this path retain the existing
    /// correctness-first per-operation lease behavior.
    fn submit_with_descriptor(
        &self,
        op: Op,
        _descriptor: DescriptorLease,
    ) -> Result<IoToken, IoError> {
        self.submit(op)
    }

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

    /// Number of descriptors retained by accepted operations or completion
    /// queues. Queried only during owner-thread termination reporting.
    fn retained_descriptor_count(&self) -> usize {
        0
    }

    /// Owner-thread acquire/release balance for descriptor leases and pending
    /// operations, published once at worker exit into
    /// [`crate::multicore::CoreTerminationSnapshot`]. See [`ResourceLedger`]
    /// for the equations. Queried only after the owner stopped mutating its
    /// tables; the default is a trivially balanced, empty ledger.
    fn resource_balance(&self) -> ResourceBalanceSnapshot {
        ResourceBalanceSnapshot::default()
    }

    /// Request owner-thread cancellation while retaining operation resources
    /// until the backend publishes its terminal cancellation CQE.
    fn cancel(&self, _token: IoToken) -> bool {
        false
    }

    /// Notify the backend that the cpu-side completion slot for `token` was
    /// fully consumed and cleared, so the token's slot index may be reissued
    /// inside a future `IoToken`.
    ///
    /// Called only from the owning shard thread, and only after the slot's
    /// waker, completion, and pending-task state has been consumed or dropped
    /// and its `token_id` zeroed — a released index can therefore never carry
    /// data for the token that released it. Backends with monotonic tokens
    /// (mio/kqueue) keep the default no-op: their slot indices are never
    /// reused. The io_uring backend overrides this to recycle cpu-side slot
    /// indices, keeping `CpuIoState::io_slots` bounded by peak in-flight
    /// operations instead of cumulative operations (ledger #45).
    fn recycle_io_slot(&self, _token: IoToken) {}

    /// Request cancellation for every accepted operation during owner-local
    /// escalation. Resources remain backend-owned until terminal drain or
    /// backend destruction after the bounded escalation window.
    fn cancel_all(&self) {}

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

    /// Whether `Op::RecvMultishot` is admitted and long-lived multishot
    /// receive completions are delivered for this backend. Readiness
    /// backends never enable it; io_uring enables it only when a provided
    /// buffer ring was successfully registered at backend creation.
    fn supports_multishot_recv(&self) -> bool {
        false
    }

    /// Pop one queued multishot-receive result for the given arm token,
    /// releasing the ring buffer that the result held (if any) back to the
    /// kernel. FIFO order: data CQEs first, the terminal result last.
    fn pop_multishot_recv_result(
        &self,
        _token: IoToken,
    ) -> Option<Result<CompletionKind, IoError>> {
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
    WriteStatic {
        fd: i32,
        data: &'static [u8],
    },
    Fsync {
        fd: i32,
    },
    /// Flush file data without the metadata that `fsync(2)` also guarantees.
    ///
    /// `fdatasync(2)` still flushes the metadata required to read the data
    /// back (file size, block extent); it does not promise mtime durability.
    /// Callers that do not need mtime to survive a crash should prefer this.
    Fdatasync {
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
    /// Arm one long-lived multishot receive on `fd`.
    ///
    /// Requires the io_uring backend with a provided buffer ring
    /// (`MINISS_URING_BUF_RING`): the kernel rejects
    /// `IORING_RECV_MULTISHOT` at prep time unless the SQE carries
    /// `IOSQE_BUFFER_SELECT`, which is only satisfiable against a registered
    /// `IORING_REGISTER_PBUF_RING` group (kernel 5.19+). The arm posts one
    /// data CQE per receive (each carrying a buffer id from the group) and a
    /// final CQE without `IORING_CQE_F_MORE` on termination (peer close,
    /// ring exhausted, or error); re-arming is the caller's job.
    ///
    /// Readiness backends reject this variant with a clear error instead of
    /// silently degrading the semantics.
    RecvMultishot {
        fd: i32,
        buf_group: u16,
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
    Fsync,
    /// Terminal result of [`Op::Fdatasync`]. Distinct from `Fsync` so callers
    /// can tell which guarantee they were given, even when group commit served
    /// their request with a stronger `fsync(2)`.
    Fdatasync,
    Close,
    ReadFile {
        bytes_read: usize,
        data: Buffer,
    },
    /// Terminal result of [`Op::WriteFile`]. `data` is the caller's buffer,
    /// handed back the way [`ReadFile`](Self::ReadFile) hands back the buffer it
    /// filled; a failed write drops it with the operation.
    WriteFile {
        bytes_written: usize,
        data: Buffer,
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
    /// One delivered multishot-receive chunk.
    ///
    /// `buffer_id` is the provided-buffer-ring id the kernel picked (upper 16
    /// bits of the CQE flags). `data` is an owned copy of the delivered bytes:
    /// the backend copies out of the ring buffer at drain time and recycles
    /// the ring slot when the completion is consumed, so net-layer code never
    /// touches ring memory.
    ///
    /// An **empty** `data` payload is the terminal end-of-stream marker: the
    /// kernel posted the final CQE (`IORING_CQE_F_MORE` unset) with a zero
    /// receive result, i.e. the peer closed. Non-empty payloads are always
    /// data CQEs; termination for other reasons surfaces as `Err`.
    RecvMultishot {
        buffer_id: u16,
        data: Buffer,
    },
}

#[cfg(all(test, unix))]
mod accept_ownership_tests {
    use super::CompletionKind;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    #[test]
    fn accepted_descriptor_closes_when_completion_is_dropped() {
        let mut fds = [0; 2];
        // SAFETY: `fds` points to two writable `RawFd` slots for `pipe` to fill.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: pipe returned ownership of this descriptor to the test.
        let accepted = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let raw = accepted.as_raw_fd();
        drop(CompletionKind::Accept {
            fd: accepted,
            addr: None,
        });
        // SAFETY: `F_GETFD` on any integer is memory-safe; the point is that the fd is gone.
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

#[cfg(any(io_backend = "epoll", all(unix, not(target_os = "linux"))))]
pub mod mio_poll;

#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
pub mod uring;

#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
pub(crate) mod buf_ring;

pub mod future;

#[cfg(any(io_backend = "epoll", all(unix, not(target_os = "linux"))))]
pub use mio_poll::MioBackend;
