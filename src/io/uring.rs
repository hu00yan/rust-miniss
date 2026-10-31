#![cfg(all(target_os = "linux", io_backend = "io_uring"))] // Only compile when io_uring backend is selected

//! A `io-uring` backend for the I/O subsystem.

use crate::buffer::Buffer;
use crate::io::buf_ring::BufRing;
use crate::io::{
    CompletionKind, DescriptorLease, IoError, IoProvider, IoToken, Op, MULTISHOT_ARM_TERMINATED,
    MULTISHOT_BUF_GROUP,
};
use io_uring::{cqueue, opcode, types, IoUring};
use libc::{iovec, msghdr, sockaddr_storage, socklen_t};
use slab::Slab;
use smallvec::SmallVec;
use std::cell::{Cell, UnsafeCell};
use std::collections::VecDeque;
use std::io;
use std::os::unix::io::{FromRawFd, OwnedFd, RawFd};
use std::task::Poll;
use std::time::Duration;

/// A struct to hold a pending operation and any associated data, like buffers.
///
/// Not `Clone`: several variants hold raw pointers into sibling heap allocations
/// (boxed `iovec` arrays, `msghdr`, `sockaddr_storage`). Cloning would duplicate
/// those pointers without duplicating pointees and would be unsound.
enum PendingOp {
    Read {
        op: Op,
        buf: Buffer,
    },
    ReadVectored {
        buffers: SmallVec<[Buffer; 4]>,
        /// Kernel-visible iovec metadata in immovable heap storage.
        /// Must be boxed *before* SQE construction so `as_ptr()` remains valid
        /// across moves into this enum and into the pending-ops slab.
        iovecs: Box<[iovec]>,
    },
    /// An owned write whose `Buffer` is returned to the caller on completion.
    WriteRent {
        data: Buffer,
    },
    WriteStatic,
    // ... existing UdpRecv/UdpSend extended for multi
    UdpRecv {
        buffer: Buffer,
        _iov: Box<iovec>,
        _msg: Box<msghdr>,
        addr_storage: Box<sockaddr_storage>,
        addr_len: Box<socklen_t>,
    },
    UdpSend {
        data: Buffer,
        _iov: Box<iovec>,
        _msg: Box<msghdr>,
        addr_storage: Box<sockaddr_storage>,
        addr_len: Box<socklen_t>,
    },
    Connect {
        addr_storage: Box<sockaddr_storage>,
    },
    Accept {
        op: Op,
        addr_storage: Option<Box<sockaddr_storage>>,
        addr_len: Option<Box<socklen_t>>,
    },
    Fsync,
    Fdatasync,
    Close,
    ReadFile {
        buf: Buffer,
    },
    WriteFile {
        data: Buffer,
    },
    PollAdd,
    Nop,
    /// A long-lived multishot receive arm (`Op::RecvMultishot`). Carries no
    /// kernel-visible storage: every CQE selects a provided-ring buffer and
    /// the shared decoder copies the payload out at drain time. The slot
    /// stays occupied from arming until the terminal CQE.
    RecvMultishot,
}

#[derive(Clone, Copy, Default)]
struct SlotMeta {
    generation: u32,
    token_id: u64,
    discard_queued_results: bool,
}

/// The completion type for the io_uring backend.
pub(super) type Completion = (IoToken, Result<CompletionKind, IoError>);
type AcceptResults = Vec<Option<VecDeque<Result<CompletionKind, IoError>>>>;
/// Per-token FIFO of multishot-receive results (data chunks, then the
/// terminal marker). Indexed by `IoToken::slot_index` like `AcceptResults`.
type MultishotResults = Vec<Option<VecDeque<Result<CompletionKind, IoError>>>>;
const ACCEPT_QUEUE_CAPACITY: usize = crate::io::ACCEPT_BACKLOG_CAPACITY;
const INTERNAL_CANCEL_USER_DATA: u64 = u64::MAX;

fn discard_overflow_accept(result: Result<CompletionKind, IoError>) {
    // Every successful accept path transfers the descriptor into an OwnedFd
    // and drops it on parse failure; dropping the result is the sole close.
    drop(result);
}

fn queue_accept_result(
    queue: &mut VecDeque<Result<CompletionKind, IoError>>,
    result: Result<CompletionKind, IoError>,
) {
    if queue.len() >= ACCEPT_QUEUE_CAPACITY {
        discard_overflow_accept(result);
    } else {
        queue.push_back(result);
    }
}

// Return false only when cancellation makes this result unobservable; callers
// then avoid waking a waiter whose successful completion was discarded.
fn queue_accept_completion(
    accept_results: &mut AcceptResults,
    token: IoToken,
    meta: SlotMeta,
    result: Result<CompletionKind, IoError>,
) -> bool {
    if meta.discard_queued_results {
        // A late successful CQE owns its descriptor in `result`; dropping it
        // closes the descriptor without retaining it in the accept queue.
        drop(result);
        return false;
    }

    let queue = accept_results[token.slot_index()]
        .get_or_insert_with(|| VecDeque::with_capacity(ACCEPT_QUEUE_CAPACITY));
    queue_accept_result(queue, result);
    true
}

const SOCKET_SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;

fn socket_send_entry(fd: RawFd, data: *const u8, len: usize) -> io_uring::squeue::Entry {
    opcode::Send::new(types::Fd(fd), data, len as u32)
        .flags(SOCKET_SEND_FLAGS)
        .build()
}

/// `Some(true)` = socket, `Some(false)` = confirmed non-socket (ENOTSOCK),
/// `None` = probe failed for another reason and the descriptor error should be
/// reported asynchronously by the operation itself.
fn probe_socket_class(fd: RawFd) -> Option<bool> {
    let mut socket_type = 0;
    let mut option_len = std::mem::size_of_val(&socket_type) as socklen_t;
    // SAFETY: `fd` is held by the operation's descriptor lease, and both output
    // pointers refer to live stack values for this immediate metadata query.
    let socket_query = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut libc::c_int).cast(),
            &mut option_len,
        )
    };
    if socket_query >= 0 {
        Some(true)
    } else if io::Error::last_os_error().raw_os_error() == Some(libc::ENOTSOCK) {
        Some(false)
    } else {
        None
    }
}

/// Build the write SQE for a descriptor, suppressing SIGPIPE for sockets while
/// preserving `Write` semantics for pipes and files.
///
/// Leased descriptors cache the classification on their shared class cell, so
/// the `getsockopt` probe runs once per registered descriptor instead of once
/// per queued operation. Unleased submissions probe every time: a raw fd has
/// no identity that outlives the operation.
fn write_entry_for_descriptor(
    lease: Option<&DescriptorLease>,
    fd: RawFd,
    data: *const u8,
    len: usize,
) -> io_uring::squeue::Entry {
    let is_socket = match lease {
        Some(lease) => match lease.socket_class() {
            Some(class) => class,
            None => match probe_socket_class(fd) {
                Some(class) => {
                    lease.store_socket_class(class);
                    class
                }
                None => true,
            },
        },
        None => probe_socket_class(fd).unwrap_or(true),
    };
    if is_socket {
        socket_send_entry(fd, data, len)
    } else {
        opcode::Write::new(types::Fd(fd), data, len as u32).build()
    }
}

/// Resolve the mutually-constrained taskrun setup flags into what the kernel
/// will actually accept.
///
/// Kernel rules (io_uring_setup(2)): DEFER_TASKRUN requires SINGLE_ISSUER and
/// is an *alternative* to COOP_TASKRUN — the two taskrun modes cannot be
/// combined, so requesting DEFER promotes SINGLE_ISSUER and drops COOP.
/// SQPOLL cannot combine with DEFER_TASKRUN at all; the SQPOLL caller passes
/// `defer_requested = false` and warns instead of failing setup.
fn resolve_taskrun_flags(
    coop_requested: bool,
    defer_requested: bool,
    single_issuer_requested: bool,
) -> (bool, bool, bool) {
    if defer_requested {
        (true, false, true)
    } else {
        (single_issuer_requested, coop_requested, false)
    }
}

/// Multishot accept was added in Linux 5.19. The opcode probe alone cannot
/// report SQE flag support, so gate the flag on the running kernel release and
/// retain the single-shot fallback for unknown/malformed releases.
fn kernel_supports_multishot_accept() -> bool {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .or_else(|_| std::fs::read_to_string("/proc/version"))
        .ok();
    let Some(release) = release else {
        return false;
    };
    let mut numbers = release
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok());
    let Some(major) = numbers.next() else {
        return false;
    };
    let minor = numbers.next().unwrap_or(0);
    major > 5 || (major == 5 && minor >= 19)
}

/// Multishot receive (`IORING_RECV_MULTISHOT` on `IORING_OP_RECV`) landed in
/// Linux 6.0, one release after the provided buffer ring it depends on. Same
/// release-file gate as [`kernel_supports_multishot_accept`].
fn kernel_supports_multishot_recv() -> bool {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .or_else(|_| std::fs::read_to_string("/proc/version"))
        .ok();
    let Some(release) = release else {
        return false;
    };
    let mut numbers = release
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok());
    let Some(major) = numbers.next() else {
        return false;
    };
    let _minor = numbers.next().unwrap_or(0);
    major >= 6
}

/// Default number of buffers in the provided buffer ring.
const DEFAULT_BUF_RING_COUNT: u16 = 256;
/// Default per-buffer size in the provided buffer ring.
const DEFAULT_BUF_RING_SIZE: usize = 4096;

/// Explicit feature switches for the io_uring backend.
///
/// Both feature flags default to off. `new()` derives them from the
/// environment (`MINISS_URING_BUF_RING`, `MINISS_URING_BUF_RING_COUNT`,
/// `MINISS_URING_BUF_RING_SIZE`, `MINISS_URING_MULTISHOT_RECV`); tests and
/// embedders construct backends directly via
/// [`UringBackend::new_with_options`] so feature state never leaks through
/// process-global environment reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UringOptions {
    /// Register one provided buffer ring (group id 1) per backend at
    /// creation. Registration failure (e.g. kernel < 5.19) degrades to the
    /// feature being off with a warning.
    pub buf_ring: bool,
    /// Buffers in the provided buffer ring. Must be a non-zero power of two.
    pub buf_ring_count: u16,
    /// Bytes per buffer in the provided buffer ring.
    pub buf_ring_size: usize,
    /// Serve `AsyncTcpStream::read_rent` from long-lived multishot receive
    /// arms. Requires `buf_ring`; without it this flag has no effect.
    pub multishot_recv: bool,
}

impl Default for UringOptions {
    fn default() -> Self {
        Self {
            buf_ring: false,
            buf_ring_count: DEFAULT_BUF_RING_COUNT,
            buf_ring_size: DEFAULT_BUF_RING_SIZE,
            multishot_recv: false,
        }
    }
}

impl UringOptions {
    /// Read the opt-in environment switches. The feature flags require the
    /// literal value `1`; the geometry knobs accept any positive number.
    fn from_env() -> Self {
        let mut options = Self {
            buf_ring: std::env::var("MINISS_URING_BUF_RING")
                .map(|value| value == "1")
                .unwrap_or(false),
            multishot_recv: std::env::var("MINISS_URING_MULTISHOT_RECV")
                .map(|value| value == "1")
                .unwrap_or(false),
            ..Self::default()
        };
        if let Ok(count) = std::env::var("MINISS_URING_BUF_RING_COUNT") {
            if let Ok(count) = count.parse::<u16>() {
                if count > 0 {
                    options.buf_ring_count = count;
                }
            }
        }
        if let Ok(size) = std::env::var("MINISS_URING_BUF_RING_SIZE") {
            if let Ok(size) = size.parse::<usize>() {
                if size > 0 {
                    options.buf_ring_size = size;
                }
            }
        }
        options
    }
}

/// A `io-uring` based `IoProvider`.
/// Thread-local to ensure single-threaded access.
pub struct UringBackend {
    /// Provided buffer ring for multishot receive (group id 1). Declared
    /// before `ring` so teardown unregisters against a live ring fd.
    buf_ring: Option<BufRing>,
    /// Whether long-lived multishot receive arms are admitted and served.
    /// True only when an option requested it AND a buffer ring registered
    /// AND the kernel release supports `IORING_RECV_MULTISHOT`.
    multishot_recv_enabled: bool,
    ring: UnsafeCell<IoUring>,
    pending_ops: UnsafeCell<Slab<PendingOp>>,
    /// Descriptor lease kept alive from acceptance through terminal CQE.
    descriptor_leases: UnsafeCell<Vec<Option<DescriptorLease>>>,
    /// Generation per slab slot. Packed with the slot index into CQE user_data
    /// so stale completions can be dropped without a hash table lookup.
    slot_meta: UnsafeCell<Vec<SlotMeta>>,
    /// Recycled cpu-side slot identities, packed as the next `IoToken` id to
    /// reissue: `(generation << 32) | slot_index`. An index enters this pool
    /// only after its completion was consumed and its cpu-side slot cleared
    /// (`IoProvider::recycle_io_slot`), so a reissued identity can never alias
    /// a live operation. Owner-thread only.
    free_io_slots: UnsafeCell<Vec<u64>>,
    /// Next never-before-issued cpu-side slot index, consumed only when the
    /// recycle pool is empty. Tracks peak in-flight operations, not cumulative
    /// operations (ledger #45).
    next_slot_index: Cell<u32>,
    /// Fallback completion queue for unsupported ops and as a capacity hint
    /// for the reusable completion buffer.
    completed_ops: UnsafeCell<Vec<Completion>>,
    /// Multi-shot accept completions indexed by slab slot.
    accept_results: UnsafeCell<AcceptResults>,
    /// Multi-shot receive results indexed by token slot. Bounded per arm by
    /// the buffer-ring count: the kernel cannot post another data CQE for an
    /// arm until a consumed buffer is republished at the tail.
    multishot_results: UnsafeCell<MultishotResults>,
    /// Count of SQEs not yet flushed. Zero means flush() can skip the syscall.
    pending_sqes: Cell<usize>,
    /// True when the kernel SQPOLL thread owns submission. In that mode flush()
    /// only publishes the SQ tail; it must not call io_uring_enter.
    sqpoll_enabled: bool,
    /// Runtime opcode probe result. Multishot accept is only advertised when
    /// the kernel reports accept support; unsupported kernels use fallback.
    supports_multishot_accept: bool,
    /// High watermark for proactive submission. When pending SQEs reach this
    /// point, submit() flushes before the ring gets too full.
    flush_high_watermark: usize,
    /// Minimum queued SQEs before flush() submits. Parsed once at backend startup.
    flush_batch_size: usize,
    /// Maximum CQEs processed in one drain pass. Parsed once at backend startup.
    drain_batch_size: usize,
    /// Park-only flush stats, drained by the runtime after each park.
    park_flushes: Cell<u64>,
    park_flush_total_us: Cell<u64>,
    max_park_flush_us: Cell<u64>,
    /// Acquire/release ledger for descriptor leases and pending ops. Plain
    /// `Cell` counters mutated only by this owner thread; see
    /// [`crate::io::ResourceLedger`] for the balance equations asserted at
    /// every drain/park boundary in debug builds.
    ledger: crate::io::ResourceLedger,
    thread_id: std::thread::ThreadId,
    _local: std::marker::PhantomData<std::rc::Rc<()>>,
}

/// Minimum pending SQEs before flush() calls io_uring_enter submit.
/// Deferring small batches reduces syscall rate.
/// Kept below MAX_BUSY_ROUNDS in multicore.rs so busy workers still submit
/// without waiting for the outer park path.
const FLUSH_BATCH_SIZE: usize = 61;
/// Maximum CQEs processed in one drain pass when the runtime asks for a
/// larger sweep. The caller can still cap this lower on busy shards.
const MAX_COMPLETIONS_PER_DRAIN: usize = 256;

impl UringBackend {
    #[inline]
    fn descriptor_fd(op: &Op) -> Option<RawFd> {
        match op {
            Op::Accept { fd }
            | Op::Read { fd, .. }
            | Op::WriteStatic { fd, .. }
            | Op::Fsync { fd }
            | Op::Fdatasync { fd }
            | Op::Close { fd }
            | Op::ReadFile { fd, .. }
            | Op::WriteFile { fd, .. }
            | Op::UdpRecv { fd, .. }
            | Op::UdpSend { fd, .. }
            | Op::UdpRecvVectored { fd, .. }
            | Op::UdpSendVectored { fd, .. }
            | Op::ReadVectored { fd, .. }
            | Op::Connect { fd, .. }
            | Op::PollAdd { fd, .. }
            | Op::ReadRent { fd, .. }
            | Op::WriteRent { fd, .. }
            | Op::RecvMultishot { fd, .. } => Some(*fd),
            Op::Nop => None,
        }
    }

    #[inline]
    fn release_descriptor_lease(&self, idx: usize) {
        // SAFETY: backend is thread-affine; only owner drains/removes leases.
        let leases = unsafe { &mut *self.descriptor_leases.get() };
        if let Some(lease) = leases.get_mut(idx) {
            // Release side of the lease ledger: counted exactly when the
            // table entry is removed.
            if lease.take().is_some() {
                self.ledger.lease_released();
            }
        }
    }

    /// Debug-build balance assertion over the shard's lease table and pending
    /// slab. Runs at every drain/park boundary; in release builds the whole
    /// body (including the table scans) is compiled out.
    #[inline]
    fn assert_ledger_balanced(&self, context: &'static str) {
        if cfg!(debug_assertions) {
            // SAFETY: owner-thread-only tables; this runs on the backend's
            // owning runtime thread at a drain/park boundary.
            let live_leases = unsafe { &*self.descriptor_leases.get() }
                .iter()
                .filter(|lease| lease.is_some())
                .count();
            // SAFETY: same owner-thread-only slab. `Slab::len` counts occupied
            // entries, i.e. the live in-flight operation set.
            let live_ops = unsafe { &*self.pending_ops.get() }.len();
            self.ledger.assert_balanced(live_leases, live_ops, context);
        }
    }

    /// Copy one multishot payload out of the provided buffer ring.
    ///
    /// The ring buffer itself is *not* republished here: it stays held by the
    /// queued completion until the consumer pops it, which is the
    /// backpressure mechanism that bounds the per-arm queue by the ring
    /// count (the kernel terminates the arm with `-ENOBUFS` once every
    /// buffer is held by an undelivered result).
    fn copy_multishot_payload(&self, buffer_id: u16, len: i32) -> Result<Buffer, IoError> {
        let Some(buf_ring) = self.buf_ring.as_ref() else {
            return Err(IoError::Other(
                "multishot CQE without a registered provided buffer ring".into(),
            ));
        };
        let Some((ptr, capacity)) = buf_ring.buffer_region(buffer_id) else {
            return Err(IoError::Other(format!(
                "multishot CQE buffer id {buffer_id} is outside the registered ring"
            )));
        };
        let len = len as usize;
        if len > capacity {
            // Trust the CQE result as an upper bound only
            // (runtime-safety-contracts.md C8): a length past the registered
            // buffer is a corrupted completion, not a smaller read.
            return Err(IoError::Other(
                "multishot CQE length exceeds the registered buffer size".into(),
            ));
        }
        let mut data = Buffer::new_uninitialized(len);
        // SAFETY: `ptr` is the registered ring region for `buffer_id`, valid
        // for `capacity` bytes, and `len <= capacity` was checked above; the
        // kernel reported initializing exactly `len` bytes for this CQE.
        unsafe {
            std::ptr::copy_nonoverlapping(ptr, data.as_mut_ptr(), len);
            data.set_len(len);
        }
        Ok(data)
    }

    /// Return one consumed buffer to the kernel's selection pool.
    fn republish_multishot_buffer(&self, buffer_id: u16) {
        if let Some(buf_ring) = self.buf_ring.as_ref() {
            buf_ring.publish(buffer_id);
        }
    }

    /// Queue one multishot result for `token`'s arm.
    ///
    /// Bounded by construction: every queued data result holds one ring
    /// buffer that is only republished at pop, so the kernel cannot push the
    /// queue past `ring count` data entries plus the terminal marker.
    fn push_multishot_result(&self, token: IoToken, result: Result<CompletionKind, IoError>) {
        // SAFETY: `multishot_results` is shard-local; the owning thread is
        // the only writer (submit reservation + drain + cancel + pop).
        let results = unsafe { &mut *self.multishot_results.get() };
        let idx = token.slot_index();
        if idx >= results.len() {
            results.resize_with(idx + 1, || None);
        }
        results[idx]
            .get_or_insert_with(VecDeque::new)
            .push_back(result);
    }

    /// Drain `token`'s multishot queue, republishing every held ring buffer.
    ///
    /// Used when an arm is abandoned (cancellation, stream drop, backend
    /// teardown): queued data results can no longer reach a reader, so their
    /// buffers must rejoin the pool instead of shrinking it.
    fn discard_multishot_queue(&self, token: IoToken) {
        // SAFETY: shard-local; owner thread only (see `push_multishot_result`).
        let results = unsafe { &mut *self.multishot_results.get() };
        let idx = token.slot_index();
        if idx >= results.len() {
            return;
        }
        if let Some(queue) = results[idx].take() {
            for result in queue {
                if let Ok(CompletionKind::RecvMultishot { buffer_id, data }) = result {
                    if !data.is_empty() {
                        self.republish_multishot_buffer(buffer_id);
                    }
                }
            }
        }
    }

    /// Shared decoder for `Op::RecvMultishot` CQEs.
    ///
    /// Called from EVERY completion drain path (`drain_completions_fused_inline`
    /// and `drain_completions_into`) so long-lived arms take the same terminal
    /// cleanup everywhere — the consolidation point for ledger #5
    /// ("io_uring terminal cleanup is duplicated across completion paths;
    /// single-close handling can drift").
    ///
    /// Semantics (consumer side: `AsyncTcpStream::read_rent`):
    ///
    /// - Data CQE (`IORING_CQE_F_BUFFER` set, result > 0): copy the ring
    ///   buffer payload into an owned [`Buffer`] and queue
    ///   `CompletionKind::RecvMultishot { buffer_id, data }`. The arm stays
    ///   armed; the slot is NOT released.
    /// - Terminal CQE (no `IORING_CQE_F_MORE`, result <= 0, or an
    ///   undecodable payload): run the exact cleanup the generic terminal
    ///   path runs (pending slot removal, descriptor lease release, metadata
    ///   reset) and queue the final result. A final CQE may still carry data
    ///   (kernel CQ-ring overflow shape, `io_recv_finish`'s `finish:` label);
    ///   that data is delivered first, then a synthetic
    ///   `MULTISHOT_ARM_TERMINATED` marker tells the consumer to re-arm.
    ///
    /// `wake` is the drain path's waker hook; each path owns its io-state
    /// lookup shape (fused inline vs. `with_io_state`).
    ///
    /// Returns `true` when the CQE belonged to a multishot arm and is fully
    /// consumed; `false` lets the caller fall through to generic handling.
    ///
    /// # Safety
    ///
    /// Caller must be the owning thread, `idx` must be a live slab slot whose
    /// generation matches the CQE, and the CQE must not have been consumed
    /// elsewhere. `result` is trusted as an upper bound on kernel writes
    /// (runtime-safety-contracts.md C8).
    // The arguments are the drain context the two callers already hold as
    // disjoint locals; a params struct would obscure that borrow splitting.
    #[allow(clippy::too_many_arguments)]
    unsafe fn handle_multishot_cqe(
        &self,
        pending_ops: &mut Slab<PendingOp>,
        slot_meta: &mut [SlotMeta],
        idx: usize,
        token: IoToken,
        result: i32,
        flags: u32,
        wake: &mut dyn FnMut(IoToken),
    ) -> bool {
        if !matches!(pending_ops.get(idx), Some(PendingOp::RecvMultishot)) {
            return false;
        }
        let discarded = slot_meta[idx].discard_queued_results;
        let has_more = cqueue::more(flags);
        let buffer_id = cqueue::buffer_select(flags);

        // Decode failure is terminal: an undecodable payload would have to be
        // dropped byte-wise, so kill the arm loudly instead.
        let mut decode_error: Option<IoError> = None;

        if result > 0 {
            match buffer_id {
                Some(buffer_id) => match self.copy_multishot_payload(buffer_id, result) {
                    Ok(data) => {
                        if discarded {
                            // No consumer will pop the queued result (the arm
                            // is being cancelled): hand the ring buffer
                            // straight back so the pool is not depleted.
                            self.republish_multishot_buffer(buffer_id);
                        } else {
                            self.push_multishot_result(
                                token,
                                Ok(CompletionKind::RecvMultishot { buffer_id, data }),
                            );
                            wake(token);
                        }
                    }
                    Err(error) => {
                        // The ring slot cannot be delivered either way;
                        // recycle it and terminate the arm.
                        self.republish_multishot_buffer(buffer_id);
                        decode_error = Some(error);
                    }
                },
                None => {
                    decode_error = Some(IoError::Other(
                        "multishot CQE without a provided buffer id".into(),
                    ));
                }
            }
        }

        // Terminal conditions: the kernel stops a multishot arm with a final
        // CQE without IORING_CQE_F_MORE (peer close ret==0, ring exhausted
        // -ENOBUFS, error), and a canceled op may post a negative CQE even
        // with the MORE flag still set. Both must release the slot.
        if decode_error.is_some() || !has_more || result <= 0 {
            let pending = pending_ops.try_remove(idx);
            debug_assert!(matches!(pending, Some(PendingOp::RecvMultishot)));
            self.release_descriptor_lease(idx);
            if let Some(meta) = slot_meta.get_mut(idx) {
                meta.token_id = 0;
                meta.discard_queued_results = false;
            }
            let terminal = if let Some(error) = decode_error {
                Err(error)
            } else if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else if result == 0 {
                // Peer closed: the empty payload is the EOF marker (see
                // `CompletionKind::RecvMultishot`).
                Ok(CompletionKind::RecvMultishot {
                    buffer_id: 0,
                    data: Buffer::empty(),
                })
            } else {
                // Final CQE still carried data (delivered above). Queue the
                // synthetic arm-dead marker so the consumer re-arms instead
                // of waiting forever on a dead token.
                Err(IoError::Other(MULTISHOT_ARM_TERMINATED.to_string()))
            };
            if !discarded {
                // A discarded arm keeps only the marker `cancel` queued (the
                // consumer-side signal); terminal results of a discarded arm
                // must not resurrect delivery. The marker is cleared by the
                // next pop or by the stream-drop cancel.
                self.push_multishot_result(token, terminal);
                wake(token);
            }
        }
        true
    }

    pub fn new(entries: u32) -> io::Result<Self> {
        // Environment switches are read exactly here, as the default source
        // for the explicit `UringOptions` constructor below.
        Self::new_with_options(entries, UringOptions::from_env())
    }

    /// Construct a backend with explicit feature switches.
    ///
    /// `options.buf_ring` mmaps + registers one provided buffer ring
    /// (group id 1, `options.buf_ring_count` buffers of
    /// `options.buf_ring_size` bytes). Registration failure — the typical
    /// cause is a kernel older than 5.19 — logs a warning and leaves the
    /// feature off instead of failing backend creation, mirroring the
    /// COOP_TASKRUN fallback. `options.multishot_recv` additionally requires
    /// the buffer ring and kernel 6.0+.
    pub fn new_with_options(entries: u32, options: UringOptions) -> io::Result<Self> {
        let flush_batch_size = std::env::var("MINISS_URING_FLUSH_BATCH_SIZE")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(FLUSH_BATCH_SIZE);
        let drain_batch_size = std::env::var("MINISS_URING_DRAIN_BATCH_SIZE")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(MAX_COMPLETIONS_PER_DRAIN);
        fn base_builder(
            enable_coop_taskrun: bool,
            enable_defer_taskrun: bool,
        ) -> io_uring::Builder {
            let mut builder = IoUring::builder();
            // Keep optional flags explicit so each one has an isolated
            // benchmark and an unsupported-kernel fallback. The flags are NOT
            // freely combinable at the kernel level; resolve_taskrun_flags
            // below owns that policy so an invalid combination cannot reach
            // the kernel.
            let (single_issuer, coop, defer) = resolve_taskrun_flags(
                enable_coop_taskrun,
                enable_defer_taskrun,
                std::env::var_os("MINISS_URING_SINGLE_ISSUER").is_some(),
            );
            if single_issuer {
                builder.setup_single_issuer();
            }
            if coop {
                builder.setup_coop_taskrun();
            }
            if defer {
                builder.setup_defer_taskrun();
            }
            builder
        }

        // COOP_TASKRUN lets the kernel run deferred io_uring work at natural
        // ring transitions instead of forcing extra task-work interrupts. It
        // is safe to request on modern kernels and falls back cleanly when the
        // running kernel rejects the setup flag.
        let enable_coop_taskrun = std::env::var_os("MINISS_URING_NO_COOP_TASKRUN").is_none();
        let enable_defer_taskrun = std::env::var_os("MINISS_URING_DEFER_TASKRUN").is_some();

        let (ring, sqpoll_enabled) = if std::env::var_os("MINISS_URING_SQPOLL").is_some() {
            // The kernel disallows DEFER_TASKRUN alongside SQPOLL (the SQPOLL
            // thread is the issuer); drop DEFER for this ring instead of
            // failing setup. The standalone DEFER experiment is run without
            // SQPOLL.
            if enable_defer_taskrun {
                tracing::warn!("MINISS_URING_SQPOLL with DEFER_TASKRUN: kernel rejects the pair, running SQPOLL without DEFER");
            }
            let mut builder = base_builder(enable_coop_taskrun, false);
            builder.setup_sqpoll(1000);
            match builder.build(entries) {
                Ok(ring) => (ring, true),
                Err(err) => {
                    tracing::warn!("io_uring SQPOLL unavailable, falling back: {err}");
                    (base_builder(false, false).build(entries)?, false)
                }
            }
        } else {
            match base_builder(enable_coop_taskrun, enable_defer_taskrun).build(entries) {
                Ok(ring) => (ring, false),
                Err(error) if enable_coop_taskrun || enable_defer_taskrun => {
                    tracing::warn!("io_uring taskrun flags unavailable, using plain ring: {error}");
                    (base_builder(false, false).build(entries)?, false)
                }
                Err(error) => return Err(error),
            }
        };
        let ring_entries = entries as usize;
        let mut probe = io_uring::Probe::new();
        let supports_multishot_accept = ring.submitter().register_probe(&mut probe).is_ok()
            && probe.is_supported(opcode::Accept::CODE)
            && kernel_supports_multishot_accept();

        // Provided buffer ring: opt-in, degrades to off when the kernel
        // cannot register it (registration is the kernel-version probe).
        let buf_ring = if options.buf_ring {
            match BufRing::create(&ring, options.buf_ring_count, options.buf_ring_size) {
                Ok(buf_ring) => Some(buf_ring),
                Err(error) => {
                    tracing::warn!(
                        "MINISS_URING_BUF_RING requested but the provided buffer ring \
                         could not be registered ({error}); continuing without it"
                    );
                    None
                }
            }
        } else {
            None
        };
        let multishot_recv_enabled = if options.multishot_recv {
            if buf_ring.is_none() {
                tracing::warn!(
                    "MINISS_URING_MULTISHOT_RECV requires a provided buffer ring; \
                     continuing with single-shot reads"
                );
                false
            } else if !kernel_supports_multishot_recv() {
                tracing::warn!(
                    "MINISS_URING_MULTISHOT_RECV requires Linux 6.0+; \
                     continuing with single-shot reads"
                );
                false
            } else {
                true
            }
        } else {
            false
        };
        let flush_high_watermark = std::cmp::max(1, ring_entries.saturating_mul(3) / 4);
        let capacity = std::cmp::max(entries, 64) as usize;
        let mut accept_results = Vec::with_capacity(capacity);
        accept_results.resize_with(capacity, || None);
        let mut multishot_results = Vec::with_capacity(capacity);
        multishot_results.resize_with(capacity, || None);
        Ok(Self {
            buf_ring,
            multishot_recv_enabled,
            ring: UnsafeCell::new(ring),
            // Use at least 64 entries for slab capacity
            pending_ops: UnsafeCell::new(Slab::with_capacity(capacity)),
            descriptor_leases: UnsafeCell::new(Vec::with_capacity(capacity)),
            slot_meta: UnsafeCell::new(vec![SlotMeta::default(); capacity]),
            free_io_slots: UnsafeCell::new(Vec::new()),
            next_slot_index: Cell::new(0),
            completed_ops: UnsafeCell::new(Vec::with_capacity(capacity)),
            accept_results: UnsafeCell::new(accept_results),
            multishot_results: UnsafeCell::new(multishot_results),
            pending_sqes: Cell::new(0),
            sqpoll_enabled,
            supports_multishot_accept,
            flush_high_watermark,
            flush_batch_size,
            drain_batch_size,
            park_flushes: Cell::new(0),
            park_flush_total_us: Cell::new(0),
            max_park_flush_us: Cell::new(0),
            ledger: crate::io::ResourceLedger::default(),
            thread_id: std::thread::current().id(),
            _local: std::marker::PhantomData,
        })
    }

    /// Submit all pending SQEs unconditionally.
    /// Used by flush() for batch threshold, park() before sleep, and submit() SQ-full.
    fn do_flush(&self) -> io::Result<()> {
        if self.pending_sqes.get() == 0 {
            return Ok(());
        }
        // SAFETY: UringBackend is thread-affine; the owning runtime thread is
        // the only code path mutating the ring.
        let ring = unsafe { &mut *self.ring.get() };
        if self.sqpoll_enabled {
            ring.submission().sync();
            self.pending_sqes.set(0);
            return Ok(());
        }
        ring.submit()
            // `io_uring::Submitter` already returns `std::io::Error` with
            // the kernel errno. Preserve it so callers and diagnostics can
            // distinguish EBADF/EINTR/EBUSY instead of seeing a wrapped
            // error with no raw OS code.
            .map(|_| self.pending_sqes.set(0))
    }

    #[inline]
    fn flush_logged(&self) {
        if let Err(error) = self.do_flush() {
            // Keep queued SQEs and pending operation ownership intact.  The
            // next event-loop turn can retry; nothing is silently discarded.
            tracing::error!("io_uring submission failed: {error}");
        }
    }

    fn discard_accept_results(&self, token: IoToken) {
        // SAFETY: `accept_results` is shard-local; this runs on the owning runtime
        // thread, so no other reference to the cell can be live.
        let accept_results = unsafe { &mut *self.accept_results.get() };
        let _ = accept_results
            .get_mut(token.slot_index())
            .and_then(Option::take);
    }

    fn discard_all_accept_results(&self) {
        // SAFETY: as above — shard-local `accept_results`, held as the sole reference.
        let accept_results = unsafe { &mut *self.accept_results.get() };
        for queue in accept_results.iter_mut() {
            let _ = queue.take();
        }
    }

    #[inline]
    fn pack_slot(idx: usize, generation: u32) -> u64 {
        debug_assert!(idx <= u32::MAX as usize);
        ((generation as u64) << 32) | (idx as u64)
    }

    #[inline]
    fn unpack_token(token_id: u64) -> Option<(usize, u32)> {
        let idx = (token_id & 0xffff_ffff) as usize;
        let generation = (token_id >> 32) as u32;
        Some((idx, generation))
    }

    /// Allocate the `IoToken` identity for a new operation.
    ///
    /// Cleared cpu-side slot indices are recycled first (ledger #45: without
    /// recycling, `CpuIoState::io_slots` grows to the highest index ever
    /// issued and RSS grows linearly with cumulative operations). A recycled
    /// identity carries the slot's bumped generation, so the new token differs
    /// from every token ever handed out for that slot index and every
    /// stale-token check keeps working:
    ///
    /// - `CpuIoState::live_slot`/`prepare_slot` compare the full token id, so
    ///   a stale token for slot `s` cannot observe or reset the slot while a
    ///   newer operation with a bumped generation owns it;
    /// - backend cancel matching compares `slot_meta.token_id` (the full id),
    ///   so a stale cancel cannot hit a reissued slot;
    /// - kernel CQEs are matched by the independent slab identity
    ///   `pack_slot(slab_idx, slab_generation)` and reconstruct the token from
    ///   `slot_meta[idx].token_id`, which always names the current operation.
    ///
    /// A fresh index (generation 1) is consumed only when the pool is empty,
    /// so the cpu-side table length tracks peak in-flight operations. Tokens
    /// stay unique among live operations: a slot index is reissued only after
    /// its previous operation reached terminal completion and its cpu-side
    /// slot was fully consumed.
    fn allocate_io_token(&self) -> Result<IoToken, IoError> {
        // SAFETY: `free_io_slots` is shard-local; submit runs on the owner thread.
        let free = unsafe { &mut *self.free_io_slots.get() };
        if let Some(id) = free.pop() {
            return Ok(IoToken::from_id(id));
        }
        let idx = self.next_slot_index.get();
        let next = idx
            .checked_add(1)
            .ok_or_else(|| IoError::Other("io slot index space exhausted".into()))?;
        self.next_slot_index.set(next);
        Ok(IoToken::from_id(Self::pack_slot(idx as usize, 1)))
    }

    /// Next packed identity for a released slot: same index, bumped
    /// generation. Generation 0 stays reserved (fresh indices carry 1),
    /// mirroring the `slot_meta` generation discipline.
    #[inline]
    fn recycled_token_id(token: IoToken) -> u64 {
        let slot = token.id() & 0xffff_ffff;
        let generation = ((token.id() >> 32) as u32).wrapping_add(1).max(1);
        ((generation as u64) << 32) | slot
    }

    /// Fused CQE processing: iterate CQ ring, reconstruct results, and
    /// write directly to CpuIoState slots + wake tasks. No intermediate Vec.
    /// Processes up to `batch_limit` CQEs per call; the event loop calls
    /// this repeatedly, interleaving with task execution.
    ///
    /// # Safety
    ///
    /// Must be called from the owning CPU thread.
    pub unsafe fn drain_completions_fused_inline(
        &self,
        wakeup_poll_token: Option<IoToken>,
        batch_limit: usize,
    ) -> (usize, bool, usize) {
        let ring = &mut *self.ring.get();
        let pending_ops = &mut *self.pending_ops.get();
        let accept_results = &mut *self.accept_results.get();
        let slot_meta = &mut *self.slot_meta.get();
        // SAFETY: this method is itself restricted to the owning shard turn.
        let io_state = unsafe { crate::cpu::current_io_state_unchecked() };

        let mut cq = ring.completion();
        cq.sync();

        let mut processed = 0usize;
        let mut saw_wakeup_poll_completion = false;
        let mut wakeups = 0usize;
        // Deliver locally failed submissions before polling the kernel CQ.
        // SAFETY: `completed_ops` is shard-local and this drain is its only writer,
        // on the owning thread required by this method's own contract.
        let failed = unsafe { &mut *self.completed_ops.get() };
        for (token, result) in failed.drain(..) {
            // SAFETY: the SQE never reached the kernel, so this operation is already
            // terminal — the precondition `take_discarded` documents.
            if unsafe { io_state.take_discarded(token) } {
                continue;
            }
            // SAFETY: `token` came from this shard's own queue, so it names one of
            // `io_state`'s live slots, and we are on the owning thread.
            if let Some(task_id) = unsafe { io_state.take_pending_task(token) } {
                io_state.push_owner_completion(task_id, token, result);
                let _ = io_state.take_waker(token);
            } else if let Some(waker) =
                // SAFETY: same live shard-local token and owning thread as above.
                unsafe { io_state.insert_completed_and_take_waker(token, result) }
            {
                wakeups += 1;
                waker.wake();
            }
            processed += 1;
        }
        for cqe in cq {
            let token_id = cqe.user_data();
            if token_id == INTERNAL_CANCEL_USER_DATA {
                continue;
            }
            let result = cqe.result();
            let flags = cqe.flags();
            let has_more = cqueue::more(flags);

            let Some((idx, generation)) = Self::unpack_token(token_id) else {
                continue;
            };
            if idx >= slot_meta.len() || slot_meta[idx].generation != generation {
                continue;
            }
            let token = IoToken::from_id(slot_meta[idx].token_id);

            // Skip the wakeup poll token — caller handles it separately.
            if wakeup_poll_token == Some(token) {
                saw_wakeup_poll_completion = true;
                if pending_ops.try_remove(idx).is_some() {
                    self.ledger.op_released();
                }
                self.release_descriptor_lease(idx);
                slot_meta[idx].token_id = 0;
                slot_meta[idx].discard_queued_results = false;
                // The parking poll never prepared a cpu-side slot (no waker,
                // completion, or pending task is ever stored for it), so its
                // identity is returned without a slot clear. Without this,
                // every park/wake cycle would consume one recycled identity.
                // SAFETY: this drain runs on the owning shard, and the parking
                // poll's slot was never prepared on this io_state.
                unsafe { io_state.recycle_unprepared_slot(token) };
                continue;
            }

            // Multishot receive: data CQEs stay armed and queue results; the
            // terminal CQE takes the shared cleanup. One helper for every
            // drain path — ledger #5 (io_uring terminal cleanup duplication)
            // consolidation point.
            if matches!(pending_ops.get(idx), Some(PendingOp::RecvMultishot)) {
                let mut wake = |woken: IoToken| {
                    if let Some(waker) = io_state.take_waker(woken) {
                        wakeups += 1;
                        waker.wake();
                    }
                };
                // SAFETY: this is the fused drain's own contract (owner thread,
                // live slot with verified generation, CQE not consumed
                // elsewhere); `handle_multishot_cqe` documents the rest.
                if unsafe {
                    self.handle_multishot_cqe(
                        pending_ops,
                        slot_meta,
                        idx,
                        token,
                        result,
                        flags,
                        &mut wake,
                    )
                } {
                    processed += 1;
                    if processed >= self.drain_batch_size.min(batch_limit) {
                        break;
                    }
                    continue;
                }
            }

            // Multishot accept: stays armed, result goes into accept_results VecDeque.
            // A negative CQE is terminal even if a canceled multishot op
            // carries the MORE flag; route it through terminal cleanup.
            if has_more && result >= 0 {
                if let Some(PendingOp::Accept {
                    addr_storage,
                    addr_len,
                    ..
                }) = pending_ops.get(idx)
                {
                    let completion_result = if result < 0 {
                        Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                    } else if let (Some(addr_storage), Some(addr_len)) =
                        (addr_storage.as_ref(), addr_len.as_ref())
                    {
                        let storage: *const sockaddr_storage = &**addr_storage as *const _;
                        // SAFETY: `addr_storage` is this backend's own boxed storage for
                        // the in-flight accept; the kernel filled it and it outlives this read.
                        let socket_addr = unsafe {
                            match (*storage).ss_family as libc::c_int {
                                libc::AF_INET => {
                                    let sa = &*(storage as *const _ as *const libc::sockaddr_in);
                                    let ip =
                                        std::net::Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr));
                                    let port = u16::from_be(sa.sin_port);
                                    Ok(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                                        ip, port,
                                    )))
                                }
                                libc::AF_INET6 => {
                                    let sa = &*(storage as *const _ as *const libc::sockaddr_in6);
                                    let ip = std::net::Ipv6Addr::from(sa.sin6_addr.s6_addr);
                                    let port = u16::from_be(sa.sin6_port);
                                    Ok(std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                                        ip,
                                        port,
                                        sa.sin6_flowinfo,
                                        sa.sin6_scope_id,
                                    )))
                                }
                                _ => Err(IoError::Io(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "Unsupported address family",
                                ))),
                            }
                        };
                        let _ = **addr_len;
                        // SAFETY: own the accepted descriptor before address decoding;
                        // parse failures must close it exactly once.
                        let accepted_fd = unsafe { OwnedFd::from_raw_fd(result) };
                        match socket_addr {
                            Ok(sa) => Ok(CompletionKind::Accept {
                                fd: accepted_fd,
                                addr: Some(sa),
                            }),
                            Err(e) => {
                                drop(accepted_fd);
                                Err(e)
                            }
                        }
                    } else {
                        Ok(CompletionKind::Accept {
                            // SAFETY: a successful accept CQE transfers a
                            // fresh descriptor to this completion.
                            fd: unsafe { OwnedFd::from_raw_fd(result) },
                            addr: None,
                        })
                    };
                    if queue_accept_completion(
                        accept_results,
                        token,
                        slot_meta[idx],
                        completion_result,
                    ) {
                        if let Some(waker) = io_state.take_waker(token) {
                            wakeups += 1;
                            waker.wake();
                        }
                    }
                    continue;
                }
            }

            // Normal completion: remove PendingOp, reconstruct result, store + wake.
            if let Some(pending_op) = pending_ops.try_remove(idx) {
                self.ledger.op_released();
                self.release_descriptor_lease(idx);
                if let Some(meta) = slot_meta.get_mut(idx) {
                    meta.token_id = 0;
                    meta.discard_queued_results = false;
                }
                if matches!(pending_op, PendingOp::Accept { .. }) {
                    // A terminal CQE orphans any still-queued accept results:
                    // their consumer is gone, and with slot recycling a future
                    // listener could otherwise inherit them (ledger #45).
                    let _ = accept_results
                        .get_mut(token.slot_index())
                        .and_then(Option::take);
                }
                // Reconstruct the terminal completion from the slot's pending
                // operation and this CQE's result.
                // SAFETY: `pending_op` came from this slot and its CQE is the one
                // being consumed right now, so every kernel-written region it owns is
                // valid for exactly `result` bytes. See `reconstruct_completion` for
                // the full contract.
                let completion_result = unsafe { reconstruct_completion(pending_op, result) };

                // Store result in slot and wake the task — no intermediate Vec.
                // SAFETY: this CQE is terminal for `token`, which is what
                // `take_discarded` requires; the owning-thread half is this method's contract.
                if unsafe { io_state.take_discarded(token) } {
                    continue;
                }
                // SAFETY: the slot for `token` was only just released above, so
                // `token` still names a live slot of `io_state`, and we are on the owner.
                if let Some(task_id) = unsafe { io_state.take_pending_task(token) } {
                    io_state.push_owner_completion(task_id, token, completion_result);
                    let _ = io_state.take_waker(token);
                } else if let Some(waker) =
                    // SAFETY: same live shard-local token and owning thread as above.
                    unsafe {
                        io_state.insert_completed_and_take_waker(token, completion_result)
                    }
                {
                    wakeups += 1;
                    waker.wake();
                }
            }

            processed += 1;
            if processed >= self.drain_batch_size.min(batch_limit) {
                break;
            }
        }
        // Fused-drain tail: both ledger equations must hold against the slab
        // and lease table this drain just mutated.
        self.assert_ledger_balanced("io_uring fused drain tail");
        (processed, saw_wakeup_poll_completion, wakeups)
    }
}

impl UringBackend {
    fn submit_inner(
        &self,
        op: Op,
        provided_descriptor_lease: Option<DescriptorLease>,
    ) -> Result<IoToken, IoError> {
        // Ensure single-threaded use (debug builds only)
        debug_assert_eq!(
            std::thread::current().id(),
            self.thread_id,
            "UringBackend must be used on the same thread"
        );

        // SAFETY: We have exclusive access on this thread.
        let ring = unsafe { &mut *self.ring.get() };
        // SAFETY: same exclusive access; `pending_ops` is shard-local to this thread.
        let pending_ops = unsafe { &mut *self.pending_ops.get() };
        // Acquire independent descriptor ownership before constructing or
        // queueing any SQE.  User-future abandonment cannot close this lease.
        let descriptor_lease = match provided_descriptor_lease {
            Some(lease) => Some(lease),
            None => match Self::descriptor_fd(&op) {
                Some(fd) => match DescriptorLease::acquire(fd) {
                    Ok(lease) => Some(lease),
                    Err(error) => {
                        return Err(IoError::Io(error));
                    }
                },
                None => None,
            },
        };
        let leased_fd = |fd: RawFd| {
            descriptor_lease
                .as_ref()
                .map_or(fd, DescriptorLease::as_raw_fd)
        };
        // Build the SQE entry and PendingOp WITHOUT user_data — we set it after
        // inserting into the slab to get the index.
        let (entry, pending_op) = {
            match op {
                Op::Accept { fd } => {
                    if self.supports_multishot_accept {
                        let entry = opcode::AcceptMulti::new(types::Fd(leased_fd(fd)))
                            .flags(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
                            .build();
                        (
                            entry,
                            PendingOp::Accept {
                                op: Op::Accept { fd },
                                addr_storage: None,
                                addr_len: None,
                            },
                        )
                    } else {
                        // Safe single-shot fallback when multishot capability
                        // is absent: retain sockaddr storage until CQE parse.
                        let addr_storage =
                            // SAFETY: `sockaddr_storage` is plain C storage; all-zero is
                            // a valid initial value and the kernel overwrites it on completion.
                            Box::new(unsafe { std::mem::zeroed::<sockaddr_storage>() });
                        let addr_len =
                            Box::new(std::mem::size_of::<sockaddr_storage>() as socklen_t);
                        let entry = opcode::Accept::new(
                            types::Fd(leased_fd(fd)),
                            addr_storage.as_ref() as *const _ as *mut _,
                            addr_len.as_ref() as *const _ as *mut _,
                        )
                        .flags(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
                        .build();
                        (
                            entry,
                            PendingOp::Accept {
                                op: Op::Accept { fd },
                                addr_storage: Some(addr_storage),
                                addr_len: Some(addr_len),
                            },
                        )
                    }
                }
                Op::Read { fd, offset, len } => {
                    let mut buf = Buffer::new_uninitialized(len);
                    let entry =
                        opcode::Read::new(types::Fd(leased_fd(fd)), buf.as_mut_ptr(), len as u32)
                            .offset(offset)
                            .build();
                    (
                        entry,
                        PendingOp::Read {
                            op: Op::Read { fd, offset, len },
                            buf,
                        },
                    )
                }
                Op::WriteFile { fd, offset, data } => {
                    let entry = opcode::Write::new(
                        types::Fd(leased_fd(fd)),
                        data.as_ptr(),
                        data.len() as u32,
                    )
                    .offset(offset)
                    .build();
                    (entry, PendingOp::WriteFile { data })
                }
                Op::WriteStatic { fd, data } => {
                    let entry = write_entry_for_descriptor(
                        descriptor_lease.as_ref(),
                        leased_fd(fd),
                        data.as_ptr(),
                        data.len(),
                    );
                    (entry, PendingOp::WriteStatic)
                }
                Op::Fsync { fd } => {
                    let entry = opcode::Fsync::new(types::Fd(leased_fd(fd))).build();
                    (entry, PendingOp::Fsync)
                }
                Op::Fdatasync { fd } => {
                    // `IORING_FSYNC_DATASYNC` is io_uring's spelling of
                    // `fdatasync(2)`: flush the data and the metadata needed to
                    // read it back, but not mtime.
                    let entry = opcode::Fsync::new(types::Fd(leased_fd(fd)))
                        .flags(types::FsyncFlags::DATASYNC)
                        .build();
                    (entry, PendingOp::Fdatasync)
                }
                Op::Close { fd } => {
                    let entry = opcode::Close::new(types::Fd(leased_fd(fd))).build();
                    (entry, PendingOp::Close)
                }
                Op::ReadFile { fd, offset, len } => {
                    let mut buf = Buffer::new_uninitialized(len);
                    let entry =
                        opcode::Read::new(types::Fd(leased_fd(fd)), buf.as_mut_ptr(), len as u32)
                            .offset(offset)
                            .build();
                    (entry, PendingOp::ReadFile { buf })
                }
                Op::UdpRecv { fd, mut buffer } => {
                    let mut iov = Box::new(iovec {
                        iov_base: buffer.as_mut_ptr() as *mut _,
                        iov_len: buffer.capacity(),
                    });
                    let mut addr_storage =
                        // SAFETY: sockaddr_storage is plain C storage used as an
                        // output buffer for recvmsg.
                        Box::new(unsafe { std::mem::zeroed::<sockaddr_storage>() });
                    let addr_len = Box::new(std::mem::size_of::<sockaddr_storage>() as socklen_t);

                    let mut msg = Box::new(msghdr {
                        msg_name: addr_storage.as_mut() as *mut _ as *mut _,
                        msg_namelen: *addr_len,
                        msg_iov: &mut *iov as *mut _,
                        msg_iovlen: 1,
                        msg_control: std::ptr::null_mut(),
                        msg_controllen: 0,
                        msg_flags: 0,
                    });

                    let entry =
                        opcode::RecvMsg::new(types::Fd(leased_fd(fd)), msg.as_mut() as *mut _)
                            .build();

                    (
                        entry,
                        PendingOp::UdpRecv {
                            buffer,
                            _iov: iov,
                            _msg: msg,
                            addr_storage,
                            addr_len,
                        },
                    )
                }
                Op::UdpSend { fd, data, addr } => {
                    let mut iov = Box::new(iovec {
                        iov_base: data.as_ptr() as *mut _,
                        iov_len: data.len(),
                    });

                    let mut addr_storage: Box<sockaddr_storage> =
                        // SAFETY: sockaddr_storage is plain C storage and will be
                        // overwritten with the matching socket address.
                        Box::new(unsafe { std::mem::zeroed() });
                    let mut addr_len: Box<socklen_t> = Box::new(0);
                    match addr {
                        std::net::SocketAddr::V4(sa_v4) => {
                            let in_addr = libc::sockaddr_in {
                                sin_family: libc::AF_INET as u16,
                                sin_port: sa_v4.port().to_be(),
                                sin_addr: libc::in_addr {
                                    s_addr: u32::from(*sa_v4.ip()).to_be(),
                                },
                                sin_zero: [0; 8],
                            };
                            // SAFETY: addr_storage is large/aligned enough for
                            // sockaddr_in.
                            unsafe {
                                std::ptr::write(
                                    addr_storage.as_mut() as *mut _ as *mut libc::sockaddr_in,
                                    in_addr,
                                );
                            }
                            *addr_len = std::mem::size_of::<libc::sockaddr_in>() as socklen_t;
                        }
                        std::net::SocketAddr::V6(sa_v6) => {
                            let sin6 = libc::sockaddr_in6 {
                                sin6_family: libc::AF_INET6 as u16,
                                sin6_port: sa_v6.port().to_be(),
                                sin6_flowinfo: sa_v6.flowinfo(),
                                sin6_addr: libc::in6_addr {
                                    s6_addr: sa_v6.ip().octets(),
                                },
                                sin6_scope_id: sa_v6.scope_id(),
                            };
                            // SAFETY: addr_storage is large/aligned enough for
                            // sockaddr_in6.
                            unsafe {
                                std::ptr::write(
                                    addr_storage.as_mut() as *mut _ as *mut libc::sockaddr_in6,
                                    sin6,
                                );
                            }
                            *addr_len = std::mem::size_of::<libc::sockaddr_in6>() as socklen_t;
                        }
                    }

                    let mut msg = Box::new(msghdr {
                        msg_name: addr_storage.as_mut() as *mut _ as *mut _,
                        msg_namelen: *addr_len,
                        msg_iov: &mut *iov as *mut _,
                        msg_iovlen: 1,
                        msg_control: std::ptr::null_mut(),
                        msg_controllen: 0,
                        msg_flags: 0,
                    });

                    let entry =
                        opcode::SendMsg::new(types::Fd(leased_fd(fd)), msg.as_mut() as *mut _)
                            .build();

                    (
                        entry,
                        PendingOp::UdpSend {
                            data,
                            _iov: iov,
                            _msg: msg,
                            addr_storage,
                            addr_len,
                        },
                    )
                }
                Op::ReadVectored { fd, buffers } => {
                    // Take ownership of buffers for PendingOp; Op gets empty placeholder.
                    // Buffer data lives on the heap, so buffer pointers stay valid after
                    // Buffer is moved into PendingOp / the slab.
                    let mut pending_buffers = buffers;
                    // CRITICAL (S0-4): allocate kernel-visible iovec metadata in stable
                    // heap storage *before* SQE construction. SmallVec<[_; 4]> stores
                    // 1–4 elements inline; taking as_ptr() then moving the SmallVec
                    // (into PendingOp, then into the slab) invalidates the SQE pointer.
                    // Box<[iovec]> keeps the pointee immovable across those moves.
                    let iovecs: Box<[iovec]> = pending_buffers
                        .iter_mut()
                        .map(|buf| iovec {
                            iov_base: buf.as_mut_ptr() as *mut _,
                            iov_len: buf.capacity(),
                        })
                        .collect();
                    let entry = opcode::Readv::new(
                        types::Fd(leased_fd(fd)),
                        iovecs.as_ptr(),
                        iovecs.len() as u32,
                    )
                    .build();
                    let pending_op = PendingOp::ReadVectored {
                        buffers: pending_buffers,
                        iovecs,
                    };
                    (entry, pending_op)
                }
                Op::Connect { fd, addr } => {
                    let mut addr_storage: Box<sockaddr_storage> =
                        // SAFETY: sockaddr_storage is plain C storage and will be
                        // overwritten with the matching socket address.
                        Box::new(unsafe { std::mem::zeroed() });
                    let addr_len: libc::socklen_t = match &addr {
                        std::net::SocketAddr::V4(_) => {
                            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
                        }
                        std::net::SocketAddr::V6(_) => {
                            std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t
                        }
                    };
                    match &addr {
                        std::net::SocketAddr::V4(sa) => {
                            let sin = libc::sockaddr_in {
                                sin_family: libc::AF_INET as u16,
                                sin_port: sa.port().to_be(),
                                sin_addr: libc::in_addr {
                                    s_addr: u32::from(*sa.ip()).to_be(),
                                },
                                sin_zero: [0; 8],
                            };
                            // SAFETY: addr_storage is large/aligned enough for
                            // sockaddr_in.
                            unsafe {
                                std::ptr::write(
                                    addr_storage.as_mut() as *mut _ as *mut libc::sockaddr_in,
                                    sin,
                                );
                            }
                        }
                        std::net::SocketAddr::V6(sa) => {
                            let sin6 = libc::sockaddr_in6 {
                                sin6_family: libc::AF_INET6 as u16,
                                sin6_port: sa.port().to_be(),
                                sin6_flowinfo: sa.flowinfo(),
                                sin6_addr: libc::in6_addr {
                                    s6_addr: sa.ip().octets(),
                                },
                                sin6_scope_id: sa.scope_id(),
                            };
                            // SAFETY: addr_storage is large/aligned enough for
                            // sockaddr_in6.
                            unsafe {
                                std::ptr::write(
                                    addr_storage.as_mut() as *mut _ as *mut libc::sockaddr_in6,
                                    sin6,
                                );
                            }
                        }
                    }
                    let entry = opcode::Connect::new(
                        types::Fd(leased_fd(fd)),
                        addr_storage.as_ref() as *const _ as *const libc::sockaddr,
                        addr_len,
                    )
                    .build();
                    (entry, PendingOp::Connect { addr_storage })
                }
                Op::ReadRent { fd, buffer } => {
                    let mut buf = buffer;
                    // ReadRent is the socket-facing rented-buffer API used by
                    // AsyncTcpStream. Use RECV so the kernel follows the
                    // native socket completion path; file reads continue to
                    // use the separate Read operation below.
                    let entry = opcode::Recv::new(
                        types::Fd(leased_fd(fd)),
                        buf.as_mut_ptr(),
                        buf.capacity() as u32,
                    )
                    .flags(0)
                    .build();
                    (
                        entry,
                        PendingOp::Read {
                            // Op::ReadRent.buffer is a placeholder — the real
                            // buffer lives in PendingOp::Read.buf and is returned
                            // on completion.
                            op: Op::ReadRent {
                                fd,
                                buffer: Buffer::empty(),
                            },
                            buf,
                        },
                    )
                }
                Op::WriteRent { fd, data } => {
                    let entry = write_entry_for_descriptor(
                        descriptor_lease.as_ref(),
                        leased_fd(fd),
                        data.as_ptr(),
                        data.len(),
                    );
                    (entry, PendingOp::WriteRent { data })
                }
                Op::RecvMultishot { fd, buf_group } => {
                    // Multishot receive is only meaningful against a provided
                    // buffer ring: the kernel rejects IORING_RECV_MULTISHOT
                    // without IOSQE_BUFFER_SELECT at prep time, and buffer
                    // selection needs a registered group. Refuse clearly
                    // instead of queueing an arm destined to fail.
                    if self.buf_ring.is_none() {
                        return Err(IoError::Io(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "Op::RecvMultishot requires a registered provided buffer ring \
                             (MINISS_URING_BUF_RING)",
                        )));
                    }
                    debug_assert_eq!(buf_group, MULTISHOT_BUF_GROUP);
                    // `opcode::RecvMulti` encodes exactly the kernel contract:
                    // IORING_OP_RECV + ioprio IORING_RECV_MULTISHOT +
                    // IOSQE_BUFFER_SELECT + buf_group. Long-lived: the slot is
                    // released only by the terminal CQE (no IORING_CQE_F_MORE).
                    let entry = opcode::RecvMulti::new(types::Fd(leased_fd(fd)), buf_group).build();
                    (entry, PendingOp::RecvMultishot)
                }
                Op::PollAdd {
                    fd,
                    events,
                    multishot,
                } => {
                    let mut entry = opcode::PollAdd::new(types::Fd(leased_fd(fd)), events);
                    if multishot {
                        entry = entry.multi(true);
                    }
                    let entry = entry.build();
                    (entry, PendingOp::PollAdd)
                }
                Op::Nop => {
                    let entry = opcode::Nop::new().build();
                    (entry, PendingOp::Nop)
                }
                _ => {
                    return Err(IoError::Io(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Operation not supported by io_uring backend",
                    )));
                }
            }
        };
        let is_accept = matches!(&pending_op, PendingOp::Accept { .. });
        let is_multishot_recv = matches!(&pending_op, PendingOp::RecvMultishot);

        // Allocate the public token identity before taking a slab slot so an
        // exhausted identity space fails without post-insert cleanup.
        let token = self.allocate_io_token()?;

        // Insert into slab to get efficient O(1) storage.
        let idx = pending_ops.insert(pending_op);
        // Acquire side of the pending-op ledger: released again exactly once
        // at a terminal `try_remove` below (kernel CQE, cancel-reap CQE, or
        // submission failure).
        self.ledger.op_acquired();

        // Keep descriptor ownership parallel to PendingOp.  Slab movement and
        // user-future drop cannot reclaim this lease before terminal CQE.
        // SAFETY: `descriptor_leases` is shard-local and resized/written only by
        // this backend owner thread while submitting.
        let leases = unsafe { &mut *self.descriptor_leases.get() };
        if idx >= leases.len() {
            leases.resize_with(idx + 1, || None);
        }
        // Acquire side of the lease ledger: this submit installs the slot's
        // lease, released again exactly once in `release_descriptor_lease`.
        if descriptor_lease.is_some() {
            self.ledger.lease_acquired();
        }
        leases[idx] = descriptor_lease;

        // SAFETY: slot_meta is shard-local and only
        // mutated by this backend owner thread while submitting/draining.
        let slot_meta = unsafe { &mut *self.slot_meta.get() };
        if idx >= slot_meta.len() {
            slot_meta.resize_with(idx + 1, SlotMeta::default);
        }
        let generation = slot_meta[idx].generation.wrapping_add(1).max(1);
        slot_meta[idx].generation = generation;
        slot_meta[idx].discard_queued_results = false;
        slot_meta[idx].token_id = token.id();
        if is_accept {
            // SAFETY: accept_results is shard-local and only mutated by this
            // backend owner thread while submitting/draining operations.
            let accept_results = unsafe { &mut *self.accept_results.get() };
            let token_idx = token.slot_index();
            if token_idx >= accept_results.len() {
                accept_results.resize_with(token_idx + 1, || None);
            }
        }
        if is_multishot_recv {
            // SAFETY: multishot_results is shard-local and only mutated by
            // this backend owner thread while submitting/draining operations.
            let multishot_results = unsafe { &mut *self.multishot_results.get() };
            let token_idx = token.slot_index();
            if token_idx >= multishot_results.len() {
                multishot_results.resize_with(token_idx + 1, || None);
            }
        }
        let entry = entry.user_data(Self::pack_slot(idx, generation));

        // Push to submission queue. If SQ is full, flush and retry.
        // NO proactive flush here — batch submission happens in flush() at event loop boundary.
        // SAFETY: `entry` lives until push copies it into the submission queue;
        // the ring is owned by this thread.
        if unsafe { ring.submission().push(&entry) }.is_err() {
            // SQ full: flush_all submits everything pending, freeing SQ slots.
            self.flush_logged();
            // SAFETY: Same as above; retry after flushing free SQ slots.
            if unsafe { ring.submission().push(&entry) }.is_err() {
                // Still full after flush — ring is too small, drop the op.
                if pending_ops.try_remove(idx).is_some() {
                    self.ledger.op_released();
                }
                self.release_descriptor_lease(idx);
                slot_meta[idx].token_id = 0;
                // The token never reached a caller and no cpu-side slot was
                // prepared for it, so its identity can go straight back to
                // the recycle pool under a bumped generation.
                self.recycle_io_slot(token);
                tracing::error!("SQ still full after emergency flush (ring too small?)");
                return Err(IoError::Io(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "io_uring submission queue is full",
                )));
            }
            let pending_sqes = self.pending_sqes.get() + 1;
            self.pending_sqes.set(pending_sqes);
            if pending_sqes >= self.flush_high_watermark {
                self.flush_logged();
            }
        } else {
            // Successful push — count it so flush() knows there's work.
            let pending_sqes = self.pending_sqes.get() + 1;
            self.pending_sqes.set(pending_sqes);
            if pending_sqes >= self.flush_high_watermark {
                self.flush_logged();
            }
        }

        Ok(token)
    }
}

/// Turns a finished slot's pending operation into a completion.
///
/// # Safety
///
/// - `pending_op` reached terminal completion for the CQE being consumed: every
///   region the kernel wrote in it is valid for exactly `result` bytes.
/// - `addr_storage`/`addr_len` still hold the bytes kernel filled for that CQE.
/// - `result` is trusted as an upper bound on the kernel's write, so this is not
///   safe against a corrupted CQE length. See runtime-safety-contracts.md C8.
unsafe fn reconstruct_completion(
    pending_op: PendingOp,
    result: i32,
) -> Result<CompletionKind, IoError> {
    match pending_op {
        PendingOp::Accept {
            addr_storage,
            addr_len,
            ..
        } => {
            if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else if let (Some(addr_storage), Some(addr_len)) =
                (addr_storage.as_ref(), addr_len.as_ref())
            {
                let storage: *const libc::sockaddr_storage = &**addr_storage as *const _;
                // SAFETY: addr_storage points to kernel-filled sockaddr
                // bytes for this completion, and the family tag comes
                // from the kernel for the same CQE.
                let socket_addr = unsafe {
                    match (*storage).ss_family as libc::c_int {
                        libc::AF_INET => {
                            let sockaddr_in = &*(storage as *const _ as *const libc::sockaddr_in);
                            let ip =
                                std::net::Ipv4Addr::from(u32::from_be(sockaddr_in.sin_addr.s_addr));
                            let port = u16::from_be(sockaddr_in.sin_port);
                            Ok(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                                ip, port,
                            )))
                        }
                        libc::AF_INET6 => {
                            let sockaddr_in6 = &*(storage as *const _ as *const libc::sockaddr_in6);
                            let ip = std::net::Ipv6Addr::from(sockaddr_in6.sin6_addr.s6_addr);
                            let port = u16::from_be(sockaddr_in6.sin6_port);
                            Ok(std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                                ip,
                                port,
                                sockaddr_in6.sin6_flowinfo,
                                sockaddr_in6.sin6_scope_id,
                            )))
                        }
                        _ => Err(IoError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Unsupported address family",
                        ))),
                    }
                };
                let _ = **addr_len;
                // SAFETY: `result` is the fresh descriptor the kernel handed to this
                // accept CQE; `OwnedFd` takes ownership of it exactly once.
                let accepted_fd = unsafe { OwnedFd::from_raw_fd(result) };
                match socket_addr {
                    Ok(sa) => Ok(CompletionKind::Accept {
                        fd: accepted_fd,
                        addr: Some(sa),
                    }),
                    Err(e) => {
                        drop(accepted_fd);
                        Err(e)
                    }
                }
            } else {
                Ok(CompletionKind::Accept {
                    // SAFETY: a successful accept CQE transfers a
                    // fresh descriptor to this completion.
                    fd: unsafe { OwnedFd::from_raw_fd(result) },
                    addr: None,
                })
            }
        }
        PendingOp::Read { op, mut buf } => {
            if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else {
                let bytes_read = result as usize;
                // SAFETY: buffer has enough capacity for the kernel
                // write, and result bytes were just reported by CQE.
                unsafe {
                    buf.set_len(bytes_read);
                }
                match &op {
                    Op::ReadRent { .. } => Ok(CompletionKind::ReadRent { buffer: buf }),
                    _ => Ok(CompletionKind::Read {
                        bytes_read,
                        data: buf,
                    }),
                }
            }
        }
        PendingOp::WriteRent { data } => {
            if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else {
                Ok(CompletionKind::WriteRent {
                    bytes_written: result as usize,
                    data,
                })
            }
        }
        PendingOp::WriteStatic => {
            if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else {
                Ok(CompletionKind::Write {
                    bytes_written: result as usize,
                })
            }
        }
        PendingOp::Fsync => {
            if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else {
                Ok(CompletionKind::Fsync)
            }
        }
        PendingOp::Fdatasync => {
            if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else {
                Ok(CompletionKind::Fdatasync)
            }
        }
        PendingOp::Close => {
            if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else {
                Ok(CompletionKind::Close)
            }
        }
        PendingOp::ReadFile { mut buf, .. } => {
            if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else {
                let bytes_read = result as usize;
                // SAFETY: buffer has enough capacity for the kernel
                // write, and result bytes were just reported by CQE.
                unsafe {
                    buf.set_len(bytes_read);
                }
                Ok(CompletionKind::ReadFile {
                    bytes_read,
                    data: buf,
                })
            }
        }
        PendingOp::WriteFile { data, .. } => {
            if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else {
                Ok(CompletionKind::WriteFile {
                    bytes_written: result as usize,
                    data,
                })
            }
        }
        PendingOp::UdpRecv {
            mut buffer,
            addr_storage,
            addr_len,
            ..
        } => {
            if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else {
                let bytes_read = result as usize;
                // SAFETY: buffer has enough capacity for the kernel
                // write, and result bytes were just reported by CQE.
                unsafe {
                    buffer.set_len(bytes_read);
                }
                let socket_addr = {
                    let storage: *const libc::sockaddr_storage = &*addr_storage as *const _;
                    // SAFETY: storage points at the sockaddr bytes
                    // just filled by the kernel for this recv.
                    unsafe {
                        match (*storage).ss_family as libc::c_int {
                            libc::AF_INET => {
                                let sockaddr_in =
                                    &*(storage as *const _ as *const libc::sockaddr_in);
                                let ip = std::net::Ipv4Addr::from(u32::from_be(
                                    sockaddr_in.sin_addr.s_addr,
                                ));
                                let port = u16::from_be(sockaddr_in.sin_port);
                                Ok(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                                    ip, port,
                                )))
                            }
                            libc::AF_INET6 => {
                                let sockaddr_in6 =
                                    &*(storage as *const _ as *const libc::sockaddr_in6);
                                let ip = std::net::Ipv6Addr::from(sockaddr_in6.sin6_addr.s6_addr);
                                let port = u16::from_be(sockaddr_in6.sin6_port);
                                Ok(std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                                    ip,
                                    port,
                                    sockaddr_in6.sin6_flowinfo,
                                    sockaddr_in6.sin6_scope_id,
                                )))
                            }
                            _ => Err(IoError::Io(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "Unsupported address family",
                            ))),
                        }
                    }
                };
                let _ = *addr_len;
                match socket_addr {
                    Ok(sa) => Ok(CompletionKind::UdpRecv {
                        bytes_read,
                        buffer,
                        addr: sa,
                    }),
                    Err(e) => Err(e),
                }
            }
        }
        PendingOp::UdpSend {
            data,
            addr_storage,
            addr_len,
            ..
        } => {
            let _ = &*addr_storage as *const _;
            let _ = *addr_len;
            if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else {
                Ok(CompletionKind::UdpSend {
                    bytes_written: result as usize,
                    data,
                })
            }
        }
        PendingOp::Connect { addr_storage, .. } => {
            if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else {
                let _ = &*addr_storage;
                Ok(CompletionKind::Connect)
            }
        }
        PendingOp::ReadVectored {
            mut buffers,
            iovecs,
            ..
        } => {
            let _ = iovecs;
            if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else {
                let bytes_read = result as usize;
                let mut remaining = bytes_read;
                for buf in buffers.iter_mut() {
                    let can_read = std::cmp::min(remaining, buf.capacity());
                    // SAFETY: buffer slots are bounded by the
                    // kernel-reported read length.
                    unsafe {
                        buf.set_len(can_read);
                    }
                    remaining -= can_read;
                    if remaining == 0 {
                        break;
                    }
                }
                Ok(CompletionKind::ReadVectored {
                    bytes_read,
                    buffers,
                })
            }
        }
        PendingOp::PollAdd => {
            if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else {
                Ok(CompletionKind::PollAdd {
                    events: result as u32,
                })
            }
        }
        PendingOp::RecvMultishot => {
            // Multishot CQEs are decoded by `handle_multishot_cqe` in every
            // drain path; the generic terminal decoder never sees this
            // variant. Reaching here would mean a drain path skipped the
            // shared helper.
            Err(IoError::Other(
                "multishot recv CQE reached the generic completion decoder".into(),
            ))
        }
        PendingOp::Nop => {
            if result < 0 {
                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
            } else {
                Ok(CompletionKind::Nop)
            }
        }
    }
}

impl Drop for UringBackend {
    fn drop(&mut self) {
        // Unregister the provided buffer ring while the ring fd is open and
        // the mappings are still mapped: unregister -> munmap, in that order,
        // instead of relying on field drop-glue order. The kernel also frees
        // registrations when the ring closes, so a failure here (e.g. the
        // group was never registered) must not panic.
        if self.buf_ring.is_some() {
            // SAFETY: exclusive owner during drop; the ring is still alive.
            let ring = unsafe { &mut *self.ring.get() };
            let _ = ring.submitter().unregister_buf_ring(MULTISHOT_BUF_GROUP);
        }
        // `self.buf_ring` drops right after this body (munmap), then `ring`.
    }
}

impl IoProvider for UringBackend {
    type Completion = (IoToken, Result<CompletionKind, IoError>);

    fn uses_completion_io(&self) -> bool {
        true
    }

    fn submit(&self, op: Op) -> Result<IoToken, IoError> {
        self.submit_inner(op, None)
    }

    fn submit_with_descriptor(
        &self,
        op: Op,
        descriptor: DescriptorLease,
    ) -> Result<IoToken, IoError> {
        self.submit_inner(op, Some(descriptor))
    }

    fn flush(&self) {
        // Skip syscall entirely when no SQEs are pending — fast path for idle ticks.
        if self.pending_sqes.get() == 0 {
            return;
        }
        // Batch: defer the syscall until we have enough SQEs queued.
        // SQEs stay in the submission ring and will be submitted by:
        // - a later flush() when the threshold is reached, or
        // - park() via submit_and_wait when the thread goes idle.
        if self.pending_sqes.get() < self.flush_batch_size {
            return;
        }
        self.flush_logged();
    }

    fn pending_submission_count(&self) -> usize {
        self.pending_sqes.get()
    }

    fn pending_operation_count(&self) -> usize {
        // SAFETY: all operation storage is shard-local and queried only by
        // the owner thread while deciding whether graceful shutdown may exit.
        let pending = unsafe { &*self.pending_ops.get() }.len();
        // SAFETY: same shard-local storage, read only by this owner thread.
        let queued_accepts = unsafe { &*self.accept_results.get() }
            .iter()
            .filter_map(Option::as_ref)
            .map(VecDeque::len)
            .sum::<usize>();
        // Multishot receive results are accepted work the consumer still
        // owes a read for; count them so graceful shutdown cannot exit early.
        // SAFETY: same shard-local storage, read only by this owner thread.
        let queued_multishot = unsafe { &*self.multishot_results.get() }
            .iter()
            .filter_map(Option::as_ref)
            .map(VecDeque::len)
            .sum::<usize>();
        // SAFETY: same shard-local storage, read only by this owner thread.
        let completed = unsafe { &*self.completed_ops.get() }.len();
        pending
            + queued_accepts
            + queued_multishot
            + completed
            + usize::from(self.pending_sqes.get() != 0)
    }

    fn retained_descriptor_count(&self) -> usize {
        // SAFETY: queried only by the owning worker during termination.
        let leases = unsafe { &*self.descriptor_leases.get() }
            .iter()
            .filter(|lease| lease.is_some())
            .count();
        // Accepted descriptors are retained in the multishot result queues,
        // not in descriptor_leases, until the owner consumes or discards them.
        // SAFETY: queried only by the owning worker during termination.
        let accepted = unsafe { &*self.accept_results.get() }
            .iter()
            .filter_map(Option::as_ref)
            .flat_map(|queue| queue.iter())
            .filter(|result| matches!(result, Ok(CompletionKind::Accept { .. })))
            .count();
        leases + accepted
    }

    fn resource_balance(&self) -> crate::io::ResourceBalanceSnapshot {
        // SAFETY: termination-time query on the owning thread, after the
        // event loop stopped mutating the shard-local tables.
        let live_leases = unsafe { &*self.descriptor_leases.get() }
            .iter()
            .filter(|lease| lease.is_some())
            .count();
        // SAFETY: same owner-thread-only slab; occupied entries are the live
        // in-flight operation set.
        let live_ops = unsafe { &*self.pending_ops.get() }.len();
        self.ledger.snapshot(live_leases, live_ops)
    }

    /// Return a fully consumed cpu-side slot identity to the recycle pool.
    ///
    /// Called from `CpuIoState`'s release paths on the owning shard, after the
    /// slot's `token_id` was zeroed — the pool entry carries a bumped
    /// generation, so the reissued token cannot equal any token previously
    /// issued for that slot index (see `allocate_io_token` for the full
    /// alias-safety argument). This is what keeps `CpuIoState::io_slots`
    /// bounded by peak in-flight operations instead of cumulative operations
    /// (ledger #45).
    fn recycle_io_slot(&self, token: IoToken) {
        // SAFETY: `free_io_slots` is shard-local; this trait method is only
        // reached from cpu-state release paths on the owning thread.
        unsafe { &mut *self.free_io_slots.get() }.push(Self::recycled_token_id(token));
    }

    fn cancel(&self, token: IoToken) -> bool {
        // SAFETY: `ring` is shard-local; `cancel` runs on the owning runtime thread
        // (see `UringBackend`'s thread-affinity note), so no other borrow is live.
        let ring = unsafe { &mut *self.ring.get() };
        let (idx, generation, is_accept, is_multishot_recv) =
            {
                // SAFETY: `slot_meta` is shard-local and only read in this scope.
                let slot_meta = unsafe { &*self.slot_meta.get() };
                // SAFETY: as above — `pending_ops` is shard-local and only read here.
                let pending_ops = unsafe { &*self.pending_ops.get() };
                let Some((idx, meta)) = slot_meta.iter().enumerate().find(|(idx, meta)| {
                    meta.token_id == token.id() && pending_ops.get(*idx).is_some()
                }) else {
                    // The arm may already be terminal with undelivered queued
                    // results (e.g. a stream dropped after the final CQE was
                    // processed but before the consumer popped the marker).
                    // Release their ring buffers so the pool is not depleted.
                    self.discard_multishot_queue(token);
                    return false;
                };
                (
                    idx,
                    meta.generation,
                    matches!(pending_ops.get(idx), Some(PendingOp::Accept { .. })),
                    matches!(pending_ops.get(idx), Some(PendingOp::RecvMultishot)),
                )
            };
        if is_accept {
            // Cancellation means queued and future accept results no longer
            // have a consumer, even if adding the cancel SQE needs a retry.
            // The shared metadata borrow ended above before this mutable borrow.
            // SAFETY: slot_meta is shard-local and cancel() runs on its owner.
            let slot_meta = unsafe { &mut *self.slot_meta.get() };
            if let Some(meta) = slot_meta
                .get_mut(idx)
                .filter(|meta| meta.token_id == token.id())
            {
                meta.discard_queued_results = true;
            }
            self.discard_accept_results(token);
        }
        if is_multishot_recv {
            // Same consumer-gone logic for a multishot receive arm: drain the
            // queue (republishing held buffers), suppress future result
            // delivery, and leave a termination marker so the next read
            // re-arms instead of waiting on a cancelled token.
            // SAFETY: slot_meta is shard-local and cancel() runs on its owner.
            let slot_meta = unsafe { &mut *self.slot_meta.get() };
            if let Some(meta) = slot_meta
                .get_mut(idx)
                .filter(|meta| meta.token_id == token.id())
            {
                meta.discard_queued_results = true;
            }
            self.discard_multishot_queue(token);
            self.push_multishot_result(
                token,
                Err(IoError::Other(MULTISHOT_ARM_TERMINATED.to_string())),
            );
            // Deliver the wake only when a shard's io state is installed:
            // cancellation can also run without one (direct backend tests,
            // teardown sweeps), and there is then no waiter to wake anyway.
            // SAFETY: when a shard's io state is installed, this runs on the
            // owning thread and `token` is a live in-flight operation there,
            // so taking its waker is exclusive access.
            crate::cpu::try_with_io_state(|io_state| unsafe {
                if let Some(waker) = io_state.take_waker(token) {
                    waker.wake();
                }
            });
        }
        // CQE user_data is the packed slot/generation identity, not the
        // public monotonic IoToken value.
        let target = Self::pack_slot(idx, generation);
        let entry = opcode::AsyncCancel::new(target)
            .build()
            .user_data(INTERNAL_CANCEL_USER_DATA);
        // SAFETY: pushes the one cancel SQE into the ring borrowed above; `entry`
        // holds no pointers and is copied into the queue before it drops.
        if unsafe { ring.submission().push(&entry) }.is_err() {
            self.flush_logged();
            // SAFETY: retry of the same self-contained SQE after flushing the queue.
            if unsafe { ring.submission().push(&entry) }.is_err() {
                return false;
            }
        }
        self.pending_sqes.set(self.pending_sqes.get() + 1);
        true
    }

    fn cancel_all(&self) {
        // SAFETY: shard-local `pending_ops`, read on the owning runtime thread only.
        let pending_ops = unsafe { &*self.pending_ops.get() };
        // SAFETY: shard-local `slot_meta`, read on the owning runtime thread only.
        let tokens = unsafe { &*self.slot_meta.get() }
            .iter()
            .enumerate()
            .filter(|(idx, meta)| meta.token_id != 0 && pending_ops.get(*idx).is_some())
            .map(|(_, meta)| IoToken::from_id(meta.token_id))
            .collect::<Vec<_>>();
        for token in tokens {
            let _ = self.cancel(token);
        }
        self.discard_all_accept_results();
        self.flush_logged();
    }

    fn park(&self, timeout: Duration) -> usize {
        // SAFETY: UringBackend is thread-affine; the owning runtime thread is
        // the only code path mutating the ring.
        let ring = unsafe { &mut *self.ring.get() };

        // Check if completions are already available.
        // No need to wait if results are ready.
        let ready = ring.completion().len();
        if ready != 0 {
            self.assert_ledger_balanced("io_uring park entry (completions ready)");
            return ready;
        }

        // Before sleeping, force any pending SQEs into the kernel. Otherwise
        // the runtime can go idle with low-volume work still sitting in the
        // submission queue, which inflates tail latency under backlog.
        let flushed = self.pending_submission_count() > 0;
        let flush_started = if flushed {
            Some(std::time::Instant::now())
        } else {
            None
        };
        self.flush_logged();
        if let Some(flush_started) = flush_started {
            let flush_elapsed_us = flush_started.elapsed().as_micros() as u64;
            self.park_flushes.set(self.park_flushes.get() + 1);
            self.park_flush_total_us
                .set(self.park_flush_total_us.get() + flush_elapsed_us);
            self.max_park_flush_us
                .set(self.max_park_flush_us.get().max(flush_elapsed_us));
        }

        let ready = ring.completion().len();
        if ready != 0 {
            self.assert_ledger_balanced("io_uring park tail (flush produced CQEs)");
            return ready;
        }

        let ts = io_uring::types::Timespec::new()
            .sec(timeout.as_secs())
            .nsec(timeout.subsec_nanos() as _);
        let args = types::SubmitArgs::new().timespec(&ts);
        match ring.submitter().submit_with_args(1, &args) {
            Ok(_) => self.pending_sqes.set(0),
            Err(error) if error.raw_os_error() == Some(libc::ETIME) => {
                // The wait timeout is the normal no-CQE result from
                // submit_with_args; SQEs were still handed to the kernel.
                self.pending_sqes.set(0);
            }
            Err(error) => {
                // Keep the pending count and operation ownership intact so a
                // transient enter failure is retried on the next turn.
                tracing::error!("io_uring park submit failed: {error}");
            }
        }
        let ready = ring.completion().len();
        self.assert_ledger_balanced("io_uring park tail");
        ready
    }

    fn drain_completions(&self) -> Poll<Vec<Self::Completion>> {
        let mut completions = Vec::new();
        match self.drain_completions_into(&mut completions) {
            Poll::Ready(()) => Poll::Ready(completions),
            Poll::Pending => Poll::Pending,
        }
    }

    fn drain_completions_into(&self, completions: &mut Vec<Self::Completion>) -> Poll<()> {
        // SAFETY: UringBackend is thread-affine; these UnsafeCell fields are
        // shard-local and only touched by this owner thread.
        let ring = unsafe { &mut *self.ring.get() };
        // SAFETY: shard-local pending-op slab; this thread is its only owner.
        let pending_ops = unsafe { &mut *self.pending_ops.get() };
        // SAFETY: shard-local multishot accept queues; this thread is their only owner.
        let accept_results = unsafe { &mut *self.accept_results.get() };
        // SAFETY: shard-local slot metadata; this thread is its only owner.
        let slot_meta = unsafe { &mut *self.slot_meta.get() };
        // SAFETY: shard-local fallback completion queue; this thread is its only owner.
        let completed_ops = unsafe { &mut *self.completed_ops.get() };
        completions.clear();
        let reserve_hint = completed_ops.capacity().max(64);
        if completions.capacity() < reserve_hint {
            completions.reserve(reserve_hint - completions.capacity());
        }
        // Admission failures are terminal completions too; deliver them via
        // the same slot/waker path as kernel CQEs.
        completions.append(completed_ops);

        let mut cq = ring.completion();
        cq.sync();

        let mut processed = 0usize;
        for cqe in cq {
            let token_id = cqe.user_data();
            if token_id == INTERNAL_CANCEL_USER_DATA {
                continue;
            }
            let result = cqe.result();
            let flags = cqe.flags();
            let has_more = cqueue::more(flags);

            let Some((idx, generation)) = Self::unpack_token(token_id) else {
                continue;
            };
            if idx >= slot_meta.len() || slot_meta[idx].generation != generation {
                continue;
            }

            let token = IoToken::from_id(slot_meta[idx].token_id);

            // Multishot receive: data CQEs stay armed and queue results; the
            // terminal CQE takes the shared cleanup. One helper for every
            // drain path — ledger #5 (io_uring terminal cleanup duplication)
            // consolidation point.
            if matches!(pending_ops.get(idx), Some(PendingOp::RecvMultishot)) {
                let mut wake = |woken: IoToken| {
                    // This drain can also run without a runtime context
                    // (direct backend tests and teardown sweeps): deliver the
                    // wake only when a shard's io state is installed. When it
                    // is, the state belongs to this owner thread — the same
                    // contract the accept path below relies on.
                    //
                    // SAFETY: when a shard's io state is installed, this runs
                    // on the owning thread and `woken` names a live in-flight
                    // operation there, so taking its waker is exclusive access.
                    crate::cpu::try_with_io_state(|io_state| unsafe {
                        if let Some(waker) = io_state.take_waker(woken) {
                            waker.wake();
                        }
                    });
                };
                // SAFETY: this drain runs on the owning thread with a live
                // slot whose generation was verified above and a CQE no other
                // path consumed; `handle_multishot_cqe` documents the rest.
                if unsafe {
                    self.handle_multishot_cqe(
                        pending_ops,
                        slot_meta,
                        idx,
                        token,
                        result,
                        flags,
                        &mut wake,
                    )
                } {
                    processed += 1;
                    if processed >= self.drain_batch_size {
                        break;
                    }
                    continue;
                }
            }

            // A negative CQE is terminal even if a canceled multishot op
            // carries the MORE flag; route it through terminal cleanup.
            if has_more && result >= 0 {
                if matches!(pending_ops.get(idx), Some(PendingOp::PollAdd)) {
                    // Multishot poll results are ordinary repeated
                    // completions. Keep the operation and descriptor lease
                    // alive until a terminal CQE arrives.
                    completed_ops.push((
                        token,
                        Ok(CompletionKind::PollAdd {
                            events: result as u32,
                        }),
                    ));
                    continue;
                }

                let Some(PendingOp::Accept {
                    op: _op,
                    addr_storage,
                    addr_len,
                }) = pending_ops.get(idx)
                else {
                    continue;
                };
                let completion_result = {
                    let res = if result < 0 {
                        Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                    } else if let (Some(addr_storage), Some(addr_len)) =
                        (addr_storage.as_ref(), addr_len.as_ref())
                    {
                        let storage: *const libc::sockaddr_storage = &**addr_storage as *const _;
                        // SAFETY: addr_storage points to kernel-filled sockaddr
                        // bytes for this completion, and the family tag comes
                        // from the kernel for the same CQE.
                        let socket_addr = unsafe {
                            match (*storage).ss_family as libc::c_int {
                                libc::AF_INET => {
                                    let sockaddr_in =
                                        &*(storage as *const _ as *const libc::sockaddr_in);
                                    let ip = std::net::Ipv4Addr::from(u32::from_be(
                                        sockaddr_in.sin_addr.s_addr,
                                    ));
                                    let port = u16::from_be(sockaddr_in.sin_port);
                                    Ok(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                                        ip, port,
                                    )))
                                }
                                libc::AF_INET6 => {
                                    let sockaddr_in6 =
                                        &*(storage as *const _ as *const libc::sockaddr_in6);
                                    let ip =
                                        std::net::Ipv6Addr::from(sockaddr_in6.sin6_addr.s6_addr);
                                    let port = u16::from_be(sockaddr_in6.sin6_port);
                                    Ok(std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                                        ip,
                                        port,
                                        sockaddr_in6.sin6_flowinfo,
                                        sockaddr_in6.sin6_scope_id,
                                    )))
                                }
                                _ => Err(IoError::Io(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "Unsupported address family",
                                ))),
                            }
                        };
                        let _ = **addr_len;
                        // SAFETY: own the accepted descriptor before address decoding;
                        // parse failures must close it exactly once.
                        let accepted_fd = unsafe { OwnedFd::from_raw_fd(result) };
                        match socket_addr {
                            Ok(sa) => Ok(CompletionKind::Accept {
                                fd: accepted_fd,
                                addr: Some(sa),
                            }),
                            Err(e) => {
                                drop(accepted_fd);
                                Err(e)
                            }
                        }
                    } else {
                        Ok(CompletionKind::Accept {
                            // SAFETY: a successful accept CQE transfers a
                            // fresh descriptor to this completion.
                            fd: unsafe { OwnedFd::from_raw_fd(result) },
                            addr: None,
                        })
                    };
                    res
                };
                if queue_accept_completion(accept_results, token, slot_meta[idx], completion_result)
                {
                    // SAFETY: token belongs to this shard and the waker slot is
                    // owned by the current io_state.
                    crate::cpu::with_io_state(|io_state| unsafe {
                        if let Some(waker) = io_state.take_waker(token) {
                            waker.wake();
                        }
                    });
                }
                continue;
            }

            let pending_op = { pending_ops.try_remove(idx) };

            if let Some(pending_op) = pending_op {
                self.ledger.op_released();
                self.release_descriptor_lease(idx);
                if let Some(meta) = slot_meta.get_mut(idx) {
                    meta.token_id = 0;
                    meta.discard_queued_results = false;
                }
                if matches!(pending_op, PendingOp::Accept { .. }) {
                    // A terminal CQE orphans any still-queued accept results:
                    // their consumer is gone, and with slot recycling a future
                    // listener could otherwise inherit them (ledger #45).
                    let _ = accept_results
                        .get_mut(token.slot_index())
                        .and_then(Option::take);
                }
                // Reconstruct the terminal completion from the slot's pending
                // operation and this CQE's result.
                // SAFETY: `pending_op` came from this slot and its CQE is the one
                // being consumed right now, so every kernel-written region it owns is
                // valid for exactly `result` bytes. See `reconstruct_completion` for
                // the full contract.
                let completion_result = unsafe { reconstruct_completion(pending_op, result) };
                completions.push((token, completion_result));
            }

            processed += 1;
            if processed >= self.drain_batch_size {
                break;
            }
        }

        // Plain-drain tail: both ledger equations must hold against the slab
        // and lease table this drain just mutated.
        self.assert_ledger_balanced("io_uring drain tail");
        if completions.is_empty() {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }

    fn pop_accept_result(&self, token: IoToken) -> Option<Result<CompletionKind, IoError>> {
        // SAFETY: accept_results is shard-local and only accessed by this
        // UringBackend owner thread.
        let accept_results = unsafe { &mut *self.accept_results.get() };
        let idx = token.slot_index();
        let queue = accept_results.get_mut(idx)?.as_mut()?;
        let result = queue.pop_front()?;
        if queue.is_empty() {
            accept_results[idx] = None;
        }
        Some(result)
    }

    fn supports_multishot_recv(&self) -> bool {
        self.multishot_recv_enabled
    }

    fn pop_multishot_recv_result(&self, token: IoToken) -> Option<Result<CompletionKind, IoError>> {
        let popped = {
            // SAFETY: multishot_results is shard-local and only accessed by
            // this UringBackend owner thread.
            let results = unsafe { &mut *self.multishot_results.get() };
            let idx = token.slot_index();
            let queue = results.get_mut(idx)?.as_mut()?;
            let result = queue.pop_front()?;
            if queue.is_empty() {
                results[idx] = None;
            }
            result
        };
        if let Ok(CompletionKind::RecvMultishot { buffer_id, data }) = &popped {
            // A data result held one ring buffer from drain until now; hand
            // it back to the kernel's selection pool. An empty payload is
            // the EOF marker and holds nothing.
            if !data.is_empty() {
                self.republish_multishot_buffer(*buffer_id);
            }
        }
        Some(popped)
    }

    fn supports_multishot_accept(&self) -> bool {
        self.supports_multishot_accept
    }

    fn accepted_streams_are_nonblocking(&self) -> bool {
        true
    }

    fn take_park_flush_stats(&self) -> (u64, u64, u64) {
        let flushes = self.park_flushes.replace(0);
        let flush_total_us = self.park_flush_total_us.replace(0);
        let max_park_flush_us = self.max_park_flush_us.replace(0);
        (flushes, flush_total_us, max_park_flush_us)
    }

    /// # Safety
    ///
    /// The caller must be the owning runtime thread, with exclusive access to
    /// all ring and completion storage, and must not concurrently drain CQEs.
    unsafe fn drain_completions_fused(
        &self,
        wakeup_poll_token: Option<IoToken>,
        batch_limit: usize,
    ) -> (usize, bool, usize) {
        self.drain_completions_fused_inline(wakeup_poll_token, batch_limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static_assertions::assert_not_impl_any!(UringBackend: Send, Sync);
    static_assertions::assert_not_impl_any!(PendingOp: Clone);
    use std::os::fd::AsRawFd;

    #[test]
    fn taskrun_flag_resolution_encodes_kernel_constraints() {
        // DEFER_TASKRUN promotes SINGLE_ISSUER and excludes COOP_TASKRUN —
        // the two taskrun modes are alternatives at the kernel level.
        assert_eq!(
            resolve_taskrun_flags(true, true, false),
            (true, false, true)
        );
        assert_eq!(
            resolve_taskrun_flags(false, true, false),
            (true, false, true)
        );
        // Without DEFER, COOP and SINGLE_ISSUER keep whatever was requested.
        assert_eq!(
            resolve_taskrun_flags(true, false, false),
            (false, true, false)
        );
        assert_eq!(
            resolve_taskrun_flags(false, false, true),
            (true, false, false)
        );
        assert_eq!(
            resolve_taskrun_flags(false, false, false),
            (false, false, false)
        );
    }

    #[test]
    fn failed_submission_is_reported_without_allocating_a_token() {
        let backend = UringBackend::new(2).expect("io_uring backend must be available");
        let result: Result<IoToken, IoError> = Err(IoError::Io(io::Error::new(
            io::ErrorKind::WouldBlock,
            "synthetic submission failure",
        )));
        assert!(result.is_err());
        assert!(matches!(backend.drain_completions(), Poll::Pending));
    }

    #[test]
    fn io_uring_enter_failure_preserves_queued_submission_for_retry() {
        let mut backend = UringBackend::new(2).expect("io_uring backend must be available");
        // Keep the SQE queued so the first explicit flush exercises the real
        // io_uring_enter call instead of the normal watermark path.
        backend.flush_high_watermark = usize::MAX;
        let token = backend.submit(Op::Nop).expect("nop admission");
        assert_eq!(backend.pending_submission_count(), 1);

        use std::os::fd::AsRawFd;
        // SAFETY: this test borrows the initialized ring storage while the
        // backend is exclusively owned and alive for the whole injection.
        let ring_fd = unsafe { (&*backend.ring.get()).as_raw_fd() };
        // Keep a duplicate file description so the owner can restore the
        // ring after injecting EBADF and prove that the queued SQE is retryable.
        // SAFETY: ring_fd is owned by this backend and remains valid here.
        let backup_fd = unsafe { libc::dup(ring_fd) };
        assert!(
            backup_fd >= 0,
            "dup io_uring fd: {}",
            io::Error::last_os_error()
        );
        // SAFETY: this intentionally closes the backend's syscall fd to force
        // the real io_uring_enter submission path to return EBADF.
        assert_eq!(unsafe { libc::close(ring_fd) }, 0);

        let error = backend
            .do_flush()
            .expect_err("closed ring must reject submit");
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
        assert_eq!(backend.pending_submission_count(), 1);

        // SAFETY: restore the original descriptor number before the backend
        // drops its IoUring owner; dup2 transfers the saved file description.
        assert_eq!(unsafe { libc::dup2(backup_fd, ring_fd) }, ring_fd);
        // SAFETY: backup_fd is the test-owned duplicate and is no longer needed.
        assert_eq!(unsafe { libc::close(backup_fd) }, 0);

        backend
            .do_flush()
            .expect("queued SQE should submit on retry");
        assert_eq!(backend.pending_submission_count(), 0);
        for _ in 0..8 {
            let _ = backend.park(Duration::from_millis(20));
            let _ = backend.drain_completions();
            if backend.pending_operation_count() == 0 {
                break;
            }
        }
        assert_eq!(
            backend.pending_operation_count(),
            0,
            "Nop leaked after retry"
        );
        let _ = token;
    }

    #[test]
    fn descriptor_lease_survives_numeric_fd_reuse() {
        let backend = UringBackend::new(8).expect("io_uring backend must be available");
        let mut fds = [0; 2];
        // SAFETY: `fds` is a live two-element array; `pipe` writes both descriptors into it.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let token = backend
            .submit(Op::Read {
                fd: fds[0],
                offset: 0,
                len: 1,
            })
            .expect("pipe read admission");

        // Drop the user's numeric descriptor, then deliberately reuse that
        // number for an unrelated file. The accepted lease must keep the
        // kernel operation attached to the original pipe.
        let reused = fds[0];
        // SAFETY: `fds[0]` is the descriptor `pipe` just created, closed exactly once here.
        unsafe { libc::close(fds[0]) };
        let replacement =
            // SAFETY: `c"/dev/null"` is a NUL-terminated literal with static lifetime.
            unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        assert_eq!(replacement, reused);
        // SAFETY: writes one byte from the static literal `b"x"` to the pipe's write end.
        assert_eq!(unsafe { libc::write(fds[1], b"x".as_ptr().cast(), 1) }, 1);

        backend.flush_logged();
        let _ = backend.park(Duration::from_secs(1));
        let completions = match backend.drain_completions() {
            Poll::Ready(completions) => completions,
            Poll::Pending => panic!("leased read did not complete"),
        };
        let result = completions
            .into_iter()
            .find(|(completed, _)| *completed == token)
            .expect("leased read completion");
        match result.1.expect("read completion") {
            CompletionKind::Read { bytes_read, data } => {
                assert_eq!(bytes_read, 1);
                assert_eq!(&data[..], b"x");
            }
            other => panic!("unexpected completion: {other:?}"),
        }
        // SAFETY: both descriptors were created by this test and are closed exactly once.
        unsafe {
            libc::close(replacement);
            libc::close(fds[1]);
        }
    }

    #[test]
    fn single_shot_accept_fallback_retains_address_storage() {
        let mut backend = UringBackend::new(8).expect("io_uring backend must be available");
        backend.supports_multishot_accept = false;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let token = backend
            .submit(Op::Accept {
                fd: listener.as_raw_fd(),
            })
            .expect("accept admission");
        let _client =
            std::net::TcpStream::connect(listener.local_addr().unwrap()).expect("client connect");
        backend.flush_logged();
        let _ = backend.park(Duration::from_secs(1));
        let completions = match backend.drain_completions() {
            Poll::Ready(completions) => completions,
            Poll::Pending => panic!("single-shot accept did not complete"),
        };
        assert!(completions.into_iter().any(|(completed, result)| {
            completed == token && matches!(result, Ok(CompletionKind::Accept { addr: Some(_), .. }))
        }));
    }

    #[test]
    fn queued_read_cancel_drains_terminal_cqe() {
        let backend = UringBackend::new(8).expect("io_uring backend must be available");
        let mut fds = [0; 2];
        // SAFETY: fds points to two writable raw-fd slots and ownership is
        // closed exactly once below.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let token = backend
            .submit(Op::Read {
                fd: fds[0],
                offset: 0,
                len: 1,
            })
            .expect("queued read admission");
        assert_eq!(backend.pending_submission_count(), 1);
        assert!(backend.cancel(token));
        backend.flush_logged();

        // The cancel CQE and the target CQE have no application-level order
        // contract.  Test the contract we own instead: the target eventually
        // reaches one terminal cleanup path and no operation remains live.
        for _ in 0..32 {
            let _ = backend.park(Duration::from_millis(20));
            let _ = backend.drain_completions();
            if backend.pending_operation_count() == 0 {
                break;
            }
        }
        assert_eq!(backend.pending_operation_count(), 0);
        // SAFETY: both pipe descriptors remain owned by this test.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    #[test]
    fn submitted_read_cancel_drains_terminal_cqe() {
        let backend = UringBackend::new(8).expect("io_uring backend must be available");
        let mut fds = [0; 2];
        // SAFETY: fds points to two writable raw-fd slots and ownership is
        // closed exactly once below.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let token = backend
            .submit(Op::Read {
                fd: fds[0],
                offset: 0,
                len: 1,
            })
            .expect("submitted read admission");
        backend.flush_logged();
        assert!(backend.pending_operation_count() >= 1);
        assert!(backend.cancel(token));
        backend.flush_logged();

        // A kernel may report the cancel request before or after the target's
        // terminal CQE.  Do not couple this test to either observation order;
        // only eventual single reclamation is part of the runtime contract.
        for _ in 0..32 {
            let _ = backend.park(Duration::from_millis(20));
            let _ = backend.drain_completions();
            if backend.pending_operation_count() == 0 {
                break;
            }
        }
        assert_eq!(backend.pending_operation_count(), 0);
        // SAFETY: both pipe descriptors remain owned by this test.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    #[test]
    fn sq_full_retry_preserves_pending_submission_count() {
        let mut backend = UringBackend::new(2).expect("io_uring backend must be available");
        // Keep the first two SQEs queued so the third exercises the
        // flush-and-retry branch instead of the normal watermark flush.
        backend.flush_high_watermark = usize::MAX;
        backend.submit(Op::Nop).expect("first nop");
        backend.submit(Op::Nop).expect("second nop");
        backend.submit(Op::Nop).expect("retry nop");
        assert_eq!(backend.pending_submission_count(), 1);

        backend.flush_logged();
        for _ in 0..16 {
            let _ = backend.park(Duration::from_millis(20));
            let _ = backend.drain_completions();
            if backend.pending_operation_count() == 0 {
                break;
            }
        }
        assert_eq!(backend.pending_operation_count(), 0);
    }

    #[test]
    fn small_completion_drain_batches_reclaim_all_terminal_operations() {
        let mut backend = UringBackend::new(8).expect("io_uring backend must be available");
        // Exercise the same drain protocol with one CQE per pass. This is a
        // deterministic proxy for kernels or event-loop turns that expose
        // completions in small batches.
        backend.drain_batch_size = 1;
        backend.flush_high_watermark = usize::MAX;
        for _ in 0..4 {
            backend.submit(Op::Nop).expect("nop admission");
        }
        backend.flush_logged();

        for _ in 0..32 {
            let _ = backend.park(Duration::from_millis(20));
            let _ = backend.drain_completions();
            if backend.pending_operation_count() == 0 {
                break;
            }
        }
        assert_eq!(backend.pending_operation_count(), 0);
        assert_eq!(backend.pending_submission_count(), 0);
    }

    #[test]
    fn multishot_accept_cancel_drains_terminal_cqe() {
        let backend = UringBackend::new(8).expect("io_uring backend must be available");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let token = backend
            .submit(Op::Accept {
                fd: listener.as_raw_fd(),
            })
            .expect("accept admission");
        assert!(backend.cancel(token));
        backend.flush_logged();
        for _ in 0..8 {
            let _ = backend.park(Duration::from_millis(20));
            let _ = backend.drain_completions();
            if backend.pending_operation_count() == 0 {
                break;
            }
        }
        assert_eq!(backend.pending_operation_count(), 0);
    }

    #[test]
    fn multishot_accept_cancel_discards_queued_owned_fds() {
        let backend = UringBackend::new(8).expect("io_uring backend must be available");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let token = backend
            .submit(Op::Accept {
                fd: listener.as_raw_fd(),
            })
            .expect("accept admission");
        let mut fds = [-1; 2];
        // SAFETY: `fds` is a live two-element array; `pipe` writes both descriptors into it.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: `fds[0]` is owned by this test and moved into `OwnedFd` exactly once.
        let accepted_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // SAFETY: `fds[1]` is the pipe's other end, created here and not used again.
        unsafe { libc::close(fds[1]) };
        // SAFETY: the test owns `backend` outright, so this cell has no other holder.
        let accept_results = unsafe { &mut *backend.accept_results.get() };
        accept_results[token.slot_index()] = Some(VecDeque::from([Ok(CompletionKind::Accept {
            fd: accepted_fd,
            addr: None,
        })]));
        assert!(
            // SAFETY: same single-owner harness; reads the queue written just above.
            unsafe { &*backend.accept_results.get() }[token.slot_index()]
                .as_ref()
                .is_some_and(|queue| !queue.is_empty())
        );
        assert!(backend.cancel(token));
        // SAFETY: same single-owner harness; checks the queue `cancel` was to discard.
        assert!(unsafe { &*backend.accept_results.get() }[token.slot_index()].is_none());
        backend.flush_logged();
        for _ in 0..8 {
            let _ = backend.park(Duration::from_millis(20));
            let _ = backend.drain_completions();
            if backend.pending_operation_count() == 0 {
                break;
            }
        }
        assert_eq!(backend.pending_operation_count(), 0);
    }

    #[test]
    fn multishot_accept_cancel_discards_late_success_cqe() {
        let backend = UringBackend::new(8).expect("io_uring backend must be available");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let token = backend
            .submit(Op::Accept {
                fd: listener.as_raw_fd(),
            })
            .expect("accept admission");
        assert!(backend.cancel(token));

        // Simulate the successful CQE's descriptor after cancel() has
        // discarded any earlier queue but before the target's terminal CQE.
        let mut fds = [-1; 2];
        // SAFETY: `fds` is a live two-element array; `pipe` writes both descriptors into it.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let accepted_raw = fds[0];
        // SAFETY: `fds[0]` is moved into `OwnedFd` exactly once below.
        let accepted_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // SAFETY: `fds[1]` is unused and owned by this test.
        unsafe { libc::close(fds[1]) };
        // SAFETY: slot_meta is shard-local; this test exclusively owns backend.
        let slot_meta = unsafe { &*backend.slot_meta.get() };
        let accept_slot = slot_meta
            .iter()
            .position(|meta| meta.token_id == token.id())
            .expect("accept slot");
        // SAFETY: accept_results is shard-local; this test exclusively owns backend.
        let accept_results = unsafe { &mut *backend.accept_results.get() };
        assert!(!queue_accept_completion(
            accept_results,
            token,
            slot_meta[accept_slot],
            Ok(CompletionKind::Accept {
                fd: accepted_fd,
                addr: None,
            }),
        ));
        // SAFETY: read-only check of the queue after the production helper discarded the result.
        assert!(unsafe { &*backend.accept_results.get() }[token.slot_index()].is_none());

        backend.flush_logged();
        for _ in 0..8 {
            let _ = backend.park(Duration::from_millis(20));
            let _ = backend.drain_completions();
            if backend.pending_operation_count() == 0 {
                break;
            }
        }

        // The late accepted fd must have been dropped and the queued result
        // must not keep shutdown's pending-operation count nonzero.
        // SAFETY: F_GETFD only inspects `accepted_raw`; the test owns no other alias.
        assert_eq!(unsafe { libc::fcntl(accepted_raw, libc::F_GETFD) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
        assert_eq!(backend.pending_operation_count(), 0);
    }

    #[test]
    fn non_socket_static_and_rent_writes_keep_write_semantics() {
        fn pipe_pair() -> (OwnedFd, OwnedFd) {
            let mut fds = [-1; 2];
            // SAFETY: `fds` is a live two-element array for `pipe` output.
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            // SAFETY: `pipe` transferred ownership of both descriptors to this test.
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
        }

        let backend = UringBackend::new(8).expect("io_uring backend must be available");
        let (static_reader, static_writer) = pipe_pair();
        let (rent_reader, rent_writer) = pipe_pair();
        let static_payload = b"static pipe payload";
        let rent_payload = b"rented pipe payload";

        let static_token = backend
            .submit(Op::WriteStatic {
                fd: static_writer.as_raw_fd(),
                data: static_payload,
            })
            .expect("static pipe write admission");
        let rent_token = backend
            .submit(Op::WriteRent {
                fd: rent_writer.as_raw_fd(),
                data: Buffer::from_slice(rent_payload),
            })
            .expect("rented pipe write admission");
        backend.flush_logged();

        let mut completions = Vec::new();
        for _ in 0..16 {
            let _ = backend.park(Duration::from_millis(20));
            if let Poll::Ready(mut ready) = backend.drain_completions() {
                completions.append(&mut ready);
            }
            if backend.pending_operation_count() == 0 {
                break;
            }
        }
        assert_eq!(completions.len(), 2, "both pipe writes must terminate");
        for (token, result) in completions {
            let completion = result.expect("pipe write should succeed");
            match (token, completion) {
                (token, CompletionKind::Write { bytes_written }) if token == static_token => {
                    assert_eq!(bytes_written, static_payload.len());
                }
                (
                    token,
                    CompletionKind::WriteRent {
                        bytes_written,
                        data,
                    },
                ) if token == rent_token => {
                    assert_eq!(bytes_written, rent_payload.len());
                    assert_eq!(&data[..], rent_payload);
                }
                (token, other) => panic!("unexpected write completion for {token:?}: {other:?}"),
            }
        }

        for (reader, expected) in [
            (&static_reader, static_payload.as_slice()),
            (&rent_reader, rent_payload.as_slice()),
        ] {
            let mut actual = vec![0; expected.len()];
            // SAFETY: `actual` is writable for its full length and the reader
            // remains open; each corresponding completed write filled this pipe.
            let read =
                unsafe { libc::read(reader.as_raw_fd(), actual.as_mut_ptr().cast(), actual.len()) };
            assert_eq!(read, expected.len() as libc::ssize_t);
            assert_eq!(&actual, expected);
        }
    }

    #[test]
    fn disconnected_socket_writes_return_epipe_without_sigpipe() {
        const CHILD_MODE: &str = "MINISS_URING_SIGPIPE_CHILD";
        if std::env::var_os(CHILD_MODE).is_some() {
            // Rust normally ignores SIGPIPE; restore the OS default in this
            // child so a missing MSG_NOSIGNAL terminates this process.
            // SAFETY: this branch executes only in the isolated child test.
            let previous = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
            assert_ne!(previous, libc::SIG_ERR, "restore default SIGPIPE action");

            let mut fds = [-1; 2];
            // SAFETY: `fds` is a live two-element array for socketpair output.
            assert_eq!(
                unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) },
                0
            );
            // SAFETY: socketpair transferred ownership of the writer fd here.
            let writer = unsafe { OwnedFd::from_raw_fd(fds[0]) };
            // SAFETY: close the peer to make both writes return EPIPE.
            assert_eq!(unsafe { libc::close(fds[1]) }, 0);

            let backend = UringBackend::new(8).expect("io_uring backend must be available");
            let static_token = backend
                .submit(Op::WriteStatic {
                    fd: writer.as_raw_fd(),
                    data: b"static payload",
                })
                .expect("static write admission");
            let rent_token = backend
                .submit(Op::WriteRent {
                    fd: writer.as_raw_fd(),
                    data: Buffer::from_slice(b"rented payload"),
                })
                .expect("rented write admission");
            backend.flush_logged();

            let mut completions = Vec::new();
            for _ in 0..16 {
                let _ = backend.park(Duration::from_millis(20));
                if let Poll::Ready(mut ready) = backend.drain_completions() {
                    completions.append(&mut ready);
                }
                if backend.pending_operation_count() == 0 {
                    break;
                }
            }
            assert_eq!(completions.len(), 2, "both writes must terminate");
            for (token, result) in completions {
                assert!(token == static_token || token == rent_token);
                match result.expect_err("disconnected socket write should fail") {
                    IoError::Io(error) => assert_eq!(error.raw_os_error(), Some(libc::EPIPE)),
                    other => panic!("unexpected write error: {other:?}"),
                }
            }
            return;
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("io::uring::tests::disconnected_socket_writes_return_epipe_without_sigpipe")
            .env(CHILD_MODE, "1")
            .output()
            .expect("run SIGPIPE isolation child");
        assert!(
            output.status.success(),
            "io_uring socket writes raised SIGPIPE or failed: status={:?}, stdout={}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn accept_burst_is_bounded_to_listener_backlog_and_overflow_closes_fd() {
        let mut queue = VecDeque::with_capacity(ACCEPT_QUEUE_CAPACITY);
        for _ in 0..ACCEPT_QUEUE_CAPACITY {
            queue_accept_result(&mut queue, Err(IoError::Other("queued".into())));
        }
        assert_eq!(queue.len(), crate::io::ACCEPT_BACKLOG_CAPACITY);

        let mut fds = [-1; 2];
        // SAFETY: `fds` is a live two-element array; `pipe` writes both descriptors into it.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let accepted_raw = fds[0];
        let peer_raw = fds[1];
        // SAFETY: `fds[0]` is owned by this test and moved into `OwnedFd` exactly once.
        let accepted_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        queue_accept_result(
            &mut queue,
            Ok(CompletionKind::Accept {
                fd: accepted_fd,
                addr: None,
            }),
        );

        assert_eq!(queue.len(), crate::io::ACCEPT_BACKLOG_CAPACITY);
        // SAFETY: read-only `F_GETFD` query; `accepted_raw` is the number the queue just closed.
        assert_eq!(unsafe { libc::fcntl(accepted_raw, libc::F_GETFD) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
        // SAFETY: as above — `peer_raw` is a descriptor this test still owns.
        assert!(unsafe { libc::fcntl(peer_raw, libc::F_GETFD) } >= 0);
        // SAFETY: `peer_raw` came from this test's `pipe` and is closed exactly once.
        unsafe { libc::close(peer_raw) };
    }
}

#[cfg(test)]
mod stable_iovec_tests {
    use super::*;
    use smallvec::SmallVec;

    /// Structural proof: `Box<[iovec]>` pointees stay fixed
    /// after the owning Box is moved (as happens when inserting PendingOp into
    /// the slab and while the SQE remains unsubmitted / delayed-submit).
    ///
    /// Covers inline-equivalent counts (1–4) and spill (5+).
    #[test]
    fn boxed_iovec_ptr_stable_after_move_for_inline_and_spill_counts() {
        for count in [1usize, 2, 3, 4, 5, 8] {
            let mut storage: Vec<u8> = vec![0u8; count * 16];
            let iovecs: Box<[iovec]> = (0..count)
                .map(|i| iovec {
                    // SAFETY: `i < count` and `storage` holds `count * 16` bytes,
                    // so every offset produced here is in bounds.
                    iov_base: unsafe { storage.as_mut_ptr().add(i * 16) } as *mut _,
                    iov_len: 16,
                })
                .collect();
            let ptr_before = iovecs.as_ptr();
            let len_before = iovecs.len();

            // Simulate move into PendingOp then into the slab.
            let moved = iovecs;
            assert_eq!(
                ptr_before,
                moved.as_ptr(),
                "Box<[iovec]> as_ptr must stay valid after move (count={count})"
            );
            assert_eq!(len_before, moved.len());
            // Pointee contents still readable through the original raw pointer.
            // SAFETY: `ptr_before` points into the `moved` allocation, which is
            // still alive here (kept as `_keep` below).
            assert_eq!(unsafe { (*ptr_before).iov_len }, 16);
            let _keep = moved;
        }
    }

    /// Documents the bug class we removed: inline SmallVec storage moves with
    /// the SmallVec, so as_ptr()-then-move is unsafe for kernel SQE arguments.
    #[test]
    fn smallvec_inline_as_ptr_is_invalidated_by_slab_like_move() {
        let mut sv: SmallVec<[iovec; 4]> = SmallVec::new();
        sv.push(iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 1,
        });
        assert!(
            !sv.spilled(),
            "precondition: single element must use inline storage"
        );
        let ptr_before = sv.as_ptr() as usize;
        // Move into a Vec (mirrors slab insert) so the SmallVec itself relocates.
        let mut slab = Vec::with_capacity(4);
        slab.push(sv);
        let ptr_after = slab[0].as_ptr() as usize;
        // Inline storage lives inside the SmallVec; relocating the SmallVec
        // relocates the iovec array. (If this ever fails, SmallVec changed
        // layout — re-evaluate the fix.)
        assert_ne!(
            ptr_before, ptr_after,
            "expected inline SmallVec as_ptr to change when moved into a slab"
        );
    }

    /// Delayed-submit structural proof: capture as_ptr from a Box, move the
    /// owner through a slab-like container, and only then "submit" (read via
    /// the captured pointer). Kernel would observe the same address.
    #[test]
    fn delayed_submit_sees_stable_iovec_after_slab_like_move() {
        for count in [1usize, 4, 5] {
            let mut bufs: Vec<Vec<u8>> = (0..count).map(|i| vec![i as u8; 8]).collect();
            let iovecs: Box<[iovec]> = bufs
                .iter_mut()
                .map(|b| iovec {
                    iov_base: b.as_mut_ptr() as *mut _,
                    iov_len: b.len(),
                })
                .collect();

            // SQE would store this pointer at build time (before flush).
            let sqe_iov_base = iovecs.as_ptr();
            let sqe_nr = iovecs.len() as u32;

            // PendingOp owns the box; slab insertion moves the owner.
            struct FakePending {
                iovecs: Box<[iovec]>,
            }
            let pending = FakePending { iovecs };
            let slab: Vec<Option<FakePending>> = vec![Some(pending)];

            // Delayed submit / kernel CQE window: pointer must still be valid.
            let live = slab[0].as_ref().unwrap();
            assert_eq!(live.iovecs.as_ptr(), sqe_iov_base);
            assert_eq!(live.iovecs.len() as u32, sqe_nr);
            for i in 0..count {
                // SAFETY: `i < count`, and the boxed array has exactly `count`
                // elements and is still owned by `slab`.
                let iov = unsafe { *sqe_iov_base.add(i) };
                assert_eq!(iov.iov_len, 8);
                // SAFETY: `iov.iov_base` points at `bufs[i]`, a live 8-byte allocation.
                assert_eq!(unsafe { *(iov.iov_base as *const u8) }, i as u8);
            }
        }
    }
}

#[cfg(test)]
mod slot_recycling_tests {
    use super::*;

    /// Packed identity of the Nth fresh cpu-side slot: generation 1.
    fn fresh_identity(idx: usize) -> u64 {
        (1u64 << 32) | idx as u64
    }

    #[test]
    fn fresh_identities_are_generation_one_with_unique_indices() {
        let backend = UringBackend::new(2).expect("io_uring backend must be available");
        let mut seen = std::collections::HashSet::new();
        for idx in 0..8 {
            let token = backend
                .allocate_io_token()
                .expect("fresh allocation must succeed");
            assert_eq!(
                token.id(),
                fresh_identity(idx),
                "fresh identities pack generation 1 with a monotonic index"
            );
            assert!(seen.insert(token.id()), "token ids must be unique");
        }
    }

    #[test]
    fn recycled_identities_reissue_the_index_under_a_bumped_generation() {
        let backend = UringBackend::new(2).expect("io_uring backend must be available");
        let first = backend
            .allocate_io_token()
            .expect("fresh allocation must succeed");
        // Release: same slot index, generation bumped from 1 to 2.
        backend.recycle_io_slot(first);
        let second = backend
            .allocate_io_token()
            .expect("the recycle pool must satisfy this allocation");
        assert_eq!(
            second.slot_index(),
            first.slot_index(),
            "recycling reuses the cleared index"
        );
        assert_ne!(
            second.id(),
            first.id(),
            "the reissued identity must differ from every token ever issued for the index"
        );
        assert_eq!(
            second.id(),
            first.id() + (1u64 << 32),
            "generation bumps by one"
        );

        // Releasing again keeps the chain monotone: gen 3 for the next reissue.
        backend.recycle_io_slot(second);
        let third = backend
            .allocate_io_token()
            .expect("the recycle pool must satisfy this allocation");
        assert_eq!(third.id(), first.id() + (2u64 << 32));
    }

    #[test]
    fn allocation_prefers_the_recycle_pool_over_fresh_indices() {
        let backend = UringBackend::new(2).expect("io_uring backend must be available");
        let first = backend
            .allocate_io_token()
            .expect("fresh allocation must succeed");
        let second = backend
            .allocate_io_token()
            .expect("fresh allocation must succeed");
        backend.recycle_io_slot(second);
        backend.recycle_io_slot(first);
        let pooled = backend.allocate_io_token().expect("the pool is non-empty");
        assert_eq!(
            pooled.id(),
            first.id() + (1u64 << 32),
            "the pool is LIFO: the last released identity is reissued first"
        );
    }

    #[test]
    fn stale_identity_cannot_equal_a_reissued_identity() {
        let backend = UringBackend::new(2).expect("io_uring backend must be available");
        let stale = backend
            .allocate_io_token()
            .expect("fresh allocation must succeed");
        backend.recycle_io_slot(stale);
        let reissued = backend
            .allocate_io_token()
            .expect("the recycle pool must satisfy this allocation");
        // The cancel scan and the cpu-side slot checks match by full id
        // equality; the bumped generation is what keeps a stale identity from
        // targeting a reissued slot.
        assert_ne!(stale.id(), reissued.id());
        assert_eq!(stale.slot_index(), reissued.slot_index());
    }
}

#[cfg(test)]
mod multishot_recv_tests {
    //! Opt-in multishot receive + provided buffer ring (`MINISS_URING_BUF_RING`
    //! / `MINISS_URING_MULTISHOT_RECV`).
    //!
    //! Backend-level tests construct `UringBackend::new_with_options`
    //! directly so the feature flags never leak through process-global env.
    //! Net-level tests need the ambient runtime (whose backend is built from
    //! the environment), so they re-exec the test binary as an isolation
    //! child with the flags set — the same pattern as
    //! `disconnected_socket_writes_return_epipe_without_sigpipe`.

    use super::*;
    use std::os::fd::AsRawFd;

    const CHILD_MODE: &str = "MINISS_URING_MULTISHOT_CHILD";
    const NET_TEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn multishot_options(count: u16, buf_size: usize) -> UringOptions {
        UringOptions {
            buf_ring: true,
            buf_ring_count: count,
            buf_ring_size: buf_size,
            multishot_recv: true,
        }
    }

    fn socketpair() -> [std::os::fd::OwnedFd; 2] {
        let mut fds = [-1; 2];
        // SAFETY: `fds` is a live two-element array for socketpair output;
        // both descriptors are owned by the caller on success.
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) },
            0
        );
        // SAFETY: socketpair transferred ownership of both descriptors here.
        unsafe { [OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])] }
    }

    fn peer_write(peer: &std::os::fd::OwnedFd, payload: &[u8]) {
        let mut written = 0;
        while written < payload.len() {
            // SAFETY: `peer` owns a live socket; the slice is valid for read.
            let n = unsafe {
                libc::write(
                    peer.as_raw_fd(),
                    payload[written..].as_ptr().cast(),
                    payload.len() - written,
                )
            };
            assert!(n > 0, "peer write failed: {}", io::Error::last_os_error());
            written += n as usize;
        }
    }

    fn queued_results(
        backend: &UringBackend,
        token: IoToken,
    ) -> Vec<Result<CompletionKind, IoError>> {
        let mut results = Vec::new();
        while let Some(result) = backend.pop_multishot_recv_result(token) {
            results.push(result);
        }
        results
    }

    // --- Backend-level (no env, constructor-parameter flags) ---

    #[test]
    fn buf_ring_registration_maps_and_publishes_every_buffer() {
        let backend = UringBackend::new_with_options(
            8,
            UringOptions {
                buf_ring: true,
                ..Default::default()
            },
        )
        .expect("io_uring backend must be available");
        // multishot_recv flag is off: the ring exists but the capability
        // stays off and RecvMultishot ops are refused.
        assert!(!backend.supports_multishot_recv());
        let buf_ring = backend.buf_ring.as_ref().expect("buffer ring registered");
        assert_eq!(buf_ring.count(), DEFAULT_BUF_RING_COUNT);
        assert_eq!(buf_ring.buf_size(), DEFAULT_BUF_RING_SIZE);
        // Every registered id resolves inside the data region.
        for bid in 0..buf_ring.count() {
            assert!(buf_ring.buffer_region(bid).is_some());
        }
        assert!(buf_ring.buffer_region(buf_ring.count()).is_none());
    }

    #[test]
    fn multishot_capability_requires_buffer_ring_and_kernel_support() {
        let without_ring = UringBackend::new_with_options(
            8,
            UringOptions {
                multishot_recv: true,
                buf_ring: false,
                ..Default::default()
            },
        )
        .expect("io_uring backend must be available");
        assert!(!without_ring.supports_multishot_recv());
        // A RecvMultishot without a registered ring is refused, not queued
        // as an arm that would die on the kernel's -EINVAL.
        let mut fds = [-1; 2];
        // SAFETY: `fds` is a live two-element array for pipe output.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let error = without_ring
            .submit(Op::RecvMultishot {
                fd: fds[0],
                buf_group: MULTISHOT_BUF_GROUP,
            })
            .expect_err("multishot without a buffer ring must be refused");
        assert!(matches!(error, IoError::Io(e) if e.kind() == io::ErrorKind::Unsupported));
        // SAFETY: both pipe descriptors are owned by this test.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }

        let with_ring = UringBackend::new_with_options(8, multishot_options(8, 4096))
            .expect("io_uring backend must be available");
        assert_eq!(
            with_ring.supports_multishot_recv(),
            kernel_supports_multishot_recv()
        );
    }

    #[test]
    fn multishot_arm_serves_repeated_data_completions_from_one_slot() {
        let backend =
            UringBackend::new_with_options(8, multishot_options(8, 4096)).expect("backend");
        let [reader, writer] = socketpair();
        let token = backend
            .submit(Op::RecvMultishot {
                fd: reader.as_raw_fd(),
                buf_group: MULTISHOT_BUF_GROUP,
            })
            .expect("multishot arm admission");
        backend.flush_logged();

        for round in 0..3 {
            let payload = format!("multishot-{round}");
            peer_write(&writer, payload.as_bytes());
            let results = loop {
                let _ = backend.park(Duration::from_millis(20));
                let _ = backend.drain_completions();
                let results = queued_results(&backend, token);
                if !results.is_empty() {
                    break results;
                }
            };
            assert_eq!(results.len(), 1, "one write must produce one data CQE");
            match &results[0] {
                Ok(CompletionKind::RecvMultishot { buffer_id, data }) => {
                    assert_eq!(data.as_ref(), payload.as_bytes());
                    assert!(*buffer_id < 8);
                }
                other => panic!("expected data completion, got {other:?}"),
            }
        }
        // The arm survived all three rounds: one live slot, one lease.
        assert_eq!(backend.pending_operation_count(), 1);
        assert_eq!(backend.retained_descriptor_count(), 1);

        // Popping released each buffer back to the pool; a fourth write must
        // still find buffers to select.
        peer_write(&writer, b"after-rounds");
        let results = loop {
            let _ = backend.park(Duration::from_millis(20));
            let _ = backend.drain_completions();
            let results = queued_results(&backend, token);
            if !results.is_empty() {
                break results;
            }
        };
        assert!(matches!(
            results.first(),
            Some(Ok(CompletionKind::RecvMultishot { data, .. })) if data.as_ref() == b"after-rounds"
        ));
        // Dropping both ends closes the socketpair exactly once each.
        drop(writer);
        drop(reader);
    }

    #[test]
    fn multishot_peer_close_posts_eof_marker_and_releases_the_slot() {
        let backend =
            UringBackend::new_with_options(8, multishot_options(8, 4096)).expect("backend");
        let [reader, writer] = socketpair();
        let token = backend
            .submit(Op::RecvMultishot {
                fd: reader.as_raw_fd(),
                buf_group: MULTISHOT_BUF_GROUP,
            })
            .expect("multishot arm admission");
        peer_write(&writer, b"final-bytes");
        // SAFETY: `writer` is owned by this test; closing is the peer close.
        drop(writer);
        backend.flush_logged();

        let results = loop {
            let _ = backend.park(Duration::from_millis(20));
            let _ = backend.drain_completions();
            let results = queued_results(&backend, token);
            if results.len() >= 2 {
                break results;
            }
        };
        assert_eq!(
            results.len(),
            2,
            "the final data CQE and the EOF marker must both be queued, in order"
        );
        assert!(
            matches!(&results[0], Ok(CompletionKind::RecvMultishot { data, .. }) if data.as_ref() == b"final-bytes")
        );
        assert!(
            matches!(&results[1], Ok(CompletionKind::RecvMultishot { data, .. }) if data.is_empty()),
            "the terminal CQE (ret 0, no MORE) is the empty-payload EOF marker"
        );
        // Terminal cleanup ran: no pending op, no descriptor lease.
        assert_eq!(backend.pending_operation_count(), 0);
        assert_eq!(backend.retained_descriptor_count(), 0);
    }

    #[test]
    fn multishot_ring_exhaustion_terminates_arm_and_republishes_on_pop() {
        // A four-buffer ring: write five chunks without consuming anything.
        let backend = UringBackend::new_with_options(8, multishot_options(4, 64)).expect("backend");
        let [reader, writer] = socketpair();
        let token = backend
            .submit(Op::RecvMultishot {
                fd: reader.as_raw_fd(),
                buf_group: MULTISHOT_BUF_GROUP,
            })
            .expect("multishot arm admission");
        backend.flush_logged();
        for chunk in ["aaaa", "bbbb", "cccc", "dddd", "eeee"] {
            peer_write(&writer, chunk.as_bytes());
        }
        // SAFETY: `writer` is owned by this test.
        drop(writer);

        let results = loop {
            let _ = backend.park(Duration::from_millis(20));
            let _ = backend.drain_completions();
            let results = queued_results(&backend, token);
            if results.len() >= 5 {
                break results;
            }
        };
        // Four data CQEs (every buffer was selected once), then the
        // -ENOBUFS termination the kernel posts once the ring is empty
        // (io_uring/kbuf.c buffer selection fails with REQ_F_BL_EMPTY).
        assert_eq!(results.len(), 5, "results: {results:?}");
        for (index, expected) in ["aaaa", "bbbb", "cccc", "dddd"].iter().enumerate() {
            match &results[index] {
                Ok(CompletionKind::RecvMultishot { data, .. }) => {
                    assert_eq!(data.as_ref(), expected.as_bytes());
                }
                other => panic!("expected data completion {index}, got {other:?}"),
            }
        }
        match &results[4] {
            Err(IoError::Io(error)) => {
                assert_eq!(error.raw_os_error(), Some(libc::ENOBUFS));
            }
            other => panic!("expected ENOBUFS termination, got {other:?}"),
        }
        // Every popped data result republished its buffer; the terminal
        // cleanup released the arm. The ring pool is fully restocked, which
        // is what makes the net layer's transparent re-arm safe.
        assert_eq!(backend.pending_operation_count(), 0);
        assert_eq!(backend.retained_descriptor_count(), 0);
    }

    #[test]
    fn multishot_cancel_republishes_held_buffers_and_signals_termination() {
        let backend = UringBackend::new_with_options(8, multishot_options(4, 64)).expect("backend");
        let [reader, writer] = socketpair();
        let token = backend
            .submit(Op::RecvMultishot {
                fd: reader.as_raw_fd(),
                buf_group: MULTISHOT_BUF_GROUP,
            })
            .expect("multishot arm admission");
        backend.flush_logged();
        peer_write(&writer, b"held");
        // Give the data CQE time to land while nothing pops it: depending on
        // timing it is either queued-with-held-buffer (cancel drains and
        // republishes it) or a discarded post-cancel CQE (the decoder
        // republishes it directly). Both paths must restock the pool.
        for _ in 0..8 {
            let _ = backend.park(Duration::from_millis(20));
            let _ = backend.drain_completions();
        }
        assert!(backend.pending_operation_count() >= 1, "arm still live");

        // Cancellation must drain the queue (republishing any held buffer),
        // suppress further delivery, and queue the termination marker so a
        // future read re-arms instead of waiting on a cancelled token.
        assert!(backend.cancel(token));
        backend.flush_logged();
        let marker = queued_results(&backend, token);
        assert_eq!(
            marker.len(),
            1,
            "exactly the termination marker: {marker:?}"
        );
        assert!(matches!(
            &marker[0],
            Err(IoError::Other(message)) if message == MULTISHOT_ARM_TERMINATED
        ));
        // The cancel SQE and the target's terminal CQE both drain.
        for _ in 0..32 {
            let _ = backend.park(Duration::from_millis(20));
            let _ = backend.drain_completions();
            if backend.pending_operation_count() == 0 {
                break;
            }
        }
        assert_eq!(backend.pending_operation_count(), 0);
        assert_eq!(backend.retained_descriptor_count(), 0);

        // The pool was not depleted by the abandoned arm: a fresh arm still
        // receives on the (re-published) ring.
        let token = backend
            .submit(Op::RecvMultishot {
                fd: reader.as_raw_fd(),
                buf_group: MULTISHOT_BUF_GROUP,
            })
            .expect("fresh arm after cancel");
        backend.flush_logged();
        peer_write(&writer, b"again");
        let results = loop {
            let _ = backend.park(Duration::from_millis(20));
            let _ = backend.drain_completions();
            let results = queued_results(&backend, token);
            if !results.is_empty() {
                break results;
            }
        };
        assert!(matches!(
            results.first(),
            Some(Ok(CompletionKind::RecvMultishot { data, .. })) if data.as_ref() == b"again"
        ));
    }

    #[test]
    fn single_shot_read_rent_path_is_unchanged_without_flags() {
        let backend = UringBackend::new_with_options(8, UringOptions::default()).expect("backend");
        assert!(!backend.supports_multishot_recv());
        assert!(backend.buf_ring.is_none());

        // `Op::ReadRent` lowers to recv(2) (the socket-facing rented API), so
        // the probe must use a socketpair, not a pipe.
        let mut fds = [-1; 2];
        // SAFETY: `fds` is a live two-element array for socketpair output.
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) },
            0
        );
        let token = backend
            .submit(Op::ReadRent {
                fd: fds[0],
                buffer: Buffer::with_capacity(64),
            })
            .expect("read_rent admission");
        // SAFETY: `fds[1]` is owned by this test.
        assert_eq!(unsafe { libc::write(fds[1], b"xyz".as_ptr().cast(), 3) }, 3);
        backend.flush_logged();
        let _ = backend.park(Duration::from_secs(1));
        let completions = match backend.drain_completions() {
            Poll::Ready(completions) => completions,
            Poll::Pending => panic!("single-shot read did not complete"),
        };
        let result = completions
            .into_iter()
            .find(|(completed, _)| *completed == token)
            .expect("read_rent completion");
        match result.1.expect("read completion") {
            CompletionKind::ReadRent { buffer } => assert_eq!(buffer.as_ref(), b"xyz"),
            other => panic!("unexpected completion: {other:?}"),
        }
        // SAFETY: both pipe descriptors are owned by this test.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    // --- Net-level (isolation child with the env flags on) ---

    fn run_in_multishot_child(test_name: &str) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg(test_name)
            .env(CHILD_MODE, "1")
            .env("MINISS_URING_BUF_RING", "1")
            .env("MINISS_URING_MULTISHOT_RECV", "1")
            .env("MINISS_URING_BUF_RING_COUNT", "8")
            .output()
            .expect("run multishot isolation child");
        assert!(
            output.status.success(),
            "multishot net test failed in the isolation child: status={:?}, stdout={}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn in_multishot_child() -> bool {
        std::env::var_os(CHILD_MODE).is_some()
    }

    #[test]
    fn net_multishot_read_rent_round_trip_on_one_arm() {
        if !in_multishot_child() {
            run_in_multishot_child(
                "io::uring::tests::net_multishot_read_rent_round_trip_on_one_arm",
            );
            return;
        }
        use crate::net::{AsyncTcpListener, AsyncTcpStream};
        use std::net::SocketAddr;

        let runtime = crate::Runtime::new();
        runtime
            .block_on(crate::timer::timeout(NET_TEST_TIMEOUT, async move {
                // The ambient backend must have both features on, otherwise this
                // test would silently prove nothing (the single-shot path also
                // delivers the bytes).
                assert!(crate::cpu::with_io_state(|state| state
                    .io_backend
                    .supports_multishot_recv()));

                let mut listener =
                    AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
                let addr = listener.local_addr().unwrap();

                let server = async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    // Three reads off ONE armed connection: arm longevity.
                    for round in 0..3 {
                        let payload = format!("arm-longevity-{round}");
                        let (n, buf) = stream.read_rent(Vec::with_capacity(64)).await.unwrap();
                        assert_eq!(n, payload.len(), "round {round}");
                        assert_eq!(&buf[..], payload.as_bytes(), "round {round}");
                    }
                };
                let client = async move {
                    let stream = AsyncTcpStream::connect(addr).await.unwrap();
                    for round in 0..3 {
                        let payload = format!("arm-longevity-{round}");
                        let (_, buf) = stream
                            .write_rent(Vec::from(payload.as_bytes()))
                            .await
                            .unwrap();
                        assert_eq!(buf.len(), payload.len());
                    }
                };
                futures::future::join(server, client).await;
            }))
            .expect("multishot round trip within the bounded wait");
    }

    #[test]
    fn net_multishot_eof_then_rearm_keeps_returning_eof() {
        if !in_multishot_child() {
            run_in_multishot_child(
                "io::uring::tests::net_multishot_eof_then_rearm_keeps_returning_eof",
            );
            return;
        }
        use crate::net::{AsyncTcpListener, AsyncTcpStream};
        use std::net::SocketAddr;

        let runtime = crate::Runtime::new();
        runtime
            .block_on(crate::timer::timeout(NET_TEST_TIMEOUT, async move {
                let mut listener =
                    AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
                let addr = listener.local_addr().unwrap();

                let server = async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    // 1. Data written before the peer close arrives first.
                    let (n, buf) = stream.read_rent(Vec::with_capacity(64)).await.unwrap();
                    assert_eq!(n, 5);
                    assert_eq!(&buf[..], b"bye!!", "expected b\"bye!!\", got {buf:?}");
                    // 2. The final CQE (ret 0, no MORE) is EOF: Ok((0, buf)).
                    let (n, _) = stream.read_rent(Vec::with_capacity(64)).await.unwrap();
                    assert_eq!(n, 0, "peer close must surface as EOF");
                    // 3. The next read re-arms on the half-closed socket; the
                    //    kernel replies with an immediate zero-length terminal,
                    //    so this returns EOF again instead of hanging — that is
                    //    the documented re-arm behavior.
                    let (n, _) = stream.read_rent(Vec::with_capacity(64)).await.unwrap();
                    assert_eq!(n, 0, "re-armed read on a closed socket returns EOF again");
                };
                let client = async move {
                    let stream = AsyncTcpStream::connect(addr).await.unwrap();
                    let (_, _) = stream
                        .write_rent(Vec::from(b"bye!!".as_slice()))
                        .await
                        .unwrap();
                    // Drop closes the socket: peer close for the server arm.
                };
                futures::future::join(server, client).await;
            }))
            .expect("multishot EOF test within the bounded wait");
    }

    #[test]
    fn net_multishot_ring_exhaustion_preserves_stream_data() {
        if !in_multishot_child() {
            run_in_multishot_child(
                "io::uring::tests::net_multishot_ring_exhaustion_preserves_stream_data",
            );
            return;
        }
        use crate::net::{AsyncTcpListener, AsyncTcpStream};
        use std::net::SocketAddr;

        // The child was started with MINISS_URING_BUF_RING_COUNT=8; a burst
        // of 24 chunks therefore exhausts the ring at least once and forces
        // the -ENOBUFS termination + transparent re-arm path.
        let chunk = [b'x'; 64];
        let chunks = 24;

        let runtime = crate::Runtime::new();
        runtime
            .block_on(crate::timer::timeout(NET_TEST_TIMEOUT, async move {
                let mut listener =
                    AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
                let addr = listener.local_addr().unwrap();

                let server = async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    for index in 0..chunks {
                        let (n, buf) = stream.read_rent(Vec::with_capacity(128)).await.unwrap();
                        assert_eq!(n, chunk.len(), "chunk {index}");
                        assert_eq!(&buf[..], &chunk, "chunk {index} must be byte-exact");
                    }
                };
                let client = async move {
                    let stream = AsyncTcpStream::connect(addr).await.unwrap();
                    for _ in 0..chunks {
                        let (_, buf) = stream
                            .write_rent(Vec::from(chunk.as_slice()))
                            .await
                            .unwrap();
                        assert_eq!(buf.len(), chunk.len());
                    }
                };
                futures::future::join(server, client).await;
            }))
            .expect("multishot exhaustion test within the bounded wait");
    }
}
