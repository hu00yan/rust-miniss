#![cfg(any(io_backend = "epoll", all(unix, not(target_os = "linux"))))]

//! Readiness backend built on `mio` (`epoll` on Linux, `kqueue` on macOS).
//!
//! This implementation is designed for a thread-per-core architecture, serving as a
//! fallback for Unix-like systems where `io-uring` is not available. Each `MioBackend`
//! instance owns a `mio::Poll` handle and is intended for single-threaded use.
//!
//! Like the `uring` backend, this module uses `UnsafeCell` and `unsafe` trait impls
//! to manage thread-local state within the `IoBackend` trait's `&self` methods.
//!
//! ## Scope of the mio abstraction
//!
//! `mio` only normalizes *readiness notification*. Both `epoll_wait` and `kevent`
//! return the same shape there — a list of `(fd, readiness)` pairs — so one code
//! path is valid at that layer. It does **not** normalize the operation state
//! machine that lives in this module: re-arm semantics for `EPOLLONESHOT` vs
//! `EV_ONESHOT`, knote accumulation on repeated `EV_ADD`, and the
//! `EV_EOF` vs `EPOLLHUP`/`EPOLLERR` mapping all differ between the two kernels.
//! macOS therefore shares this code path but is **not** covered by independent
//! kqueue-specific tests; treat any macOS-only result as unverified here.

use crate::io::{CompletionKind, DescriptorLease, IoError, IoProvider, IoToken, Op};
use crate::Buffer;
use mio::{unix::SourceFd, Events, Interest, Poll, Token};
use std::cell::UnsafeCell;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::task::Poll as TaskPoll;
use std::time::Duration;
use threadpool::ThreadPool;

type IoCompletion = (IoToken, Result<CompletionKind, IoError>);
const COMPLETION_QUEUE_CAPACITY: usize = 4096;

/// Flush strength requested by one caller.
///
/// A group that mixes requests runs the *strongest* one, so a caller that
/// needs `fsync(2)` metadata durability is never served by a `fdatasync(2)`
/// that skipped it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FlushMode {
    /// `fdatasync(2)`: data only.
    Data,
    /// `fsync(2)`: data plus metadata.
    All,
}

impl FlushMode {
    /// Completion kind reported back to a caller that asked for this mode.
    fn completion(self) -> CompletionKind {
        match self {
            FlushMode::Data => CompletionKind::Fdatasync,
            FlushMode::All => CompletionKind::Fsync,
        }
    }
}

/// File identity used to decide that two flushes may share one syscall.
///
/// Descriptor *numbers* are not identity: a caller may close its file between
/// two submits and the number may be handed to a different file, which would
/// let one caller observe durability for a flush nobody did on its behalf.
/// `fstat` identity is stable for as long as a lease keeps the inode alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

/// One inode's pending flushes, to be served by a single syscall.
///
/// The group is closed at dispatch time: once the closure is queued, later
/// requests open a new group instead of joining, because a request that arrives
/// after the syscall started may have written data the syscall has already
/// passed.
#[derive(Debug)]
struct FlushGroup {
    identity: FileIdentity,
    mode: FlushMode,
    /// Descriptor the pool worker flushes. Holding the lease here keeps the
    /// inode (and therefore `identity`) alive independently of any one slot.
    lease: DescriptorLease,
    /// Tokens waiting on this group's syscall, leader first.
    waiters: Vec<(IoToken, FlushMode)>,
}

/// `fstat` the descriptor an accepted flush will run against.
///
/// This runs on the owner thread once per flush request. It is a metadata
/// read, not a blocking flush, and it is what makes group commit safe.
fn flush_identity(fd: RawFd) -> io::Result<FileIdentity> {
    // SAFETY: stat is a plain C storage type; fstat only writes into it.
    let mut stat: libc::stat =
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        unsafe { std::mem::zeroed() };
    // SAFETY: fd is a live descriptor held by an accepted lease, so the stat
    // call cannot outlive it.
    // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileIdentity {
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
    })
}

fn publish_read_len(buffer: &mut Buffer, bytes_read: usize) -> Result<(), IoError> {
    buffer
        .publish_read_len(bytes_read)
        .then_some(())
        .ok_or_else(|| {
            IoError::Other(format!(
                "read reported {bytes_read} bytes for {} byte buffer",
                buffer.capacity()
            ))
        })
}

/// Safe file descriptor wrapper that ensures RAII compliance
#[derive(Debug)]
struct SafeFdFile {
    file: Option<std::fs::File>,
}

impl SafeFdFile {
    /// Create a new SafeFdFile from a raw fd
    fn new(fd: RawFd) -> io::Result<Self> {
        // SAFETY: dup creates an independent owned fd; the original fd remains
        // owned by its caller and is not consumed here.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let dup_fd = unsafe { libc::dup(fd) };
        if dup_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: dup_fd is newly owned by this function and is transferred
        // into File exactly once.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
        Ok(Self { file: Some(file) })
    }

    /// Get mutable reference to the file
    fn as_mut(&mut self) -> &mut std::fs::File {
        self.file.as_mut().expect("File should be available")
    }
}

impl Drop for SafeFdFile {
    fn drop(&mut self) {
        // Ensure the file is properly closed when SafeFdFile is dropped
        if let Some(file) = self.file.take() {
            drop(file); // This will close the fd
        }
    }
}

/// A readiness-based `IoBackend` implementation using `mio`.
#[derive(Debug)]
pub struct MioBackend {
    /// The `mio::Poll` instance, which manages file descriptor readiness.
    poll: UnsafeCell<Poll>,
    /// A buffer for receiving events from `mio::Poll`.
    events: UnsafeCell<Events>,
    /// Pending operations indexed by the `mio::Token` slot.
    pending_ops: UnsafeCell<Vec<Option<(IoToken, Op)>>>,
    /// Descriptor leases acquired when an operation is accepted.  Keeping the
    /// duplicate alive through terminal completion prevents fd-number reuse
    /// from retargeting a queued fallback operation.
    descriptor_leases: UnsafeCell<Vec<Option<crate::io::DescriptorLease>>>,
    descriptor_lease_tokens: UnsafeCell<Vec<Option<IoToken>>>,
    /// UDP receive buffers indexed by the `mio::Token` slot.
    udp_recv_buffers: UnsafeCell<Vec<Option<crate::buffer::Buffer>>>,
    /// A counter to generate unique `mio::Token` values.
    next_token: UnsafeCell<usize>,
    /// Shard-local IoToken counter.
    next_io_token: std::cell::Cell<u64>,
    /// Thread pool for blocking file I/O operations
    thread_pool: Arc<ThreadPool>,
    /// Flush requests accepted but not yet handed to the file thread pool.
    /// Shard-local: only the owner thread reads or writes this list.
    flush_groups: UnsafeCell<Vec<FlushGroup>>,
    /// Per-shard counters for flush accounting. Read only by the owner thread;
    /// aggregating them is a control-plane concern (MUST #6).
    flush_requests: std::cell::Cell<u64>,
    flush_syscalls: std::cell::Cell<u64>,
    /// Same-thread completed operations.
    completed_local: UnsafeCell<Vec<IoCompletion>>,
    /// Completed operations from the file I/O thread pool.
    completed_tx: SyncSender<IoCompletion>,
    completed_rx: UnsafeCell<Receiver<IoCompletion>>,
    /// Set when `park()` filled `events` with readiness notifications that the
    /// next drain must consume. The runtime calls park() then drain, and both
    /// would otherwise call `Poll::poll` on the same kernel queue: drain's
    /// zero-timeout poll would report zero new events and drop the ones park
    /// just harvested.
    park_left_events: std::cell::Cell<bool>,
    _local: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl MioBackend {
    /// Creates a new `MioBackend`.
    pub fn new() -> io::Result<Self> {
        // Create a small thread pool for blocking I/O operations
        let pool_size = std::cmp::max(2, num_cpus::get() / 4);
        let thread_pool = Arc::new(ThreadPool::new(pool_size));
        let (completed_tx, completed_rx) = sync_channel(COMPLETION_QUEUE_CAPACITY);

        Ok(Self {
            poll: UnsafeCell::new(Poll::new()?),
            events: UnsafeCell::new(Events::with_capacity(1024)),
            pending_ops: UnsafeCell::new(Vec::with_capacity(1024)),
            descriptor_leases: UnsafeCell::new(Vec::with_capacity(1024)),
            descriptor_lease_tokens: UnsafeCell::new(Vec::with_capacity(1024)),
            udp_recv_buffers: UnsafeCell::new(Vec::with_capacity(1024)),
            next_token: UnsafeCell::new(0),
            next_io_token: std::cell::Cell::new(1),
            thread_pool,
            flush_groups: UnsafeCell::new(Vec::new()),
            flush_requests: std::cell::Cell::new(0),
            flush_syscalls: std::cell::Cell::new(0),
            completed_local: UnsafeCell::new(Vec::new()),
            completed_tx,
            completed_rx: UnsafeCell::new(completed_rx),
            park_left_events: std::cell::Cell::new(false),
            _local: std::marker::PhantomData,
        })
    }

    fn complete_local(&self, completion: IoCompletion) {
        // SAFETY: MioBackend is driven by one runtime thread; completed_local
        // is shard-local and not accessed concurrently.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let completed_local = unsafe { &mut *self.completed_local.get() };
        completed_local.push(completion);
    }

    #[inline]
    fn ensure_slot_capacity<T>(vec: &mut Vec<Option<T>>, idx: usize) {
        if idx >= vec.len() {
            vec.resize_with(idx + 1, || None);
        }
    }

    /// Number of `fsync(2)`/`fdatasync(2)` syscalls this shard has issued.
    ///
    /// Per-shard counter (AGENTS.md MUST #6). Compared against
    /// [`Self::flush_request_count`] it shows how much group commit saved.
    /// Test-only: the data plane reads it nowhere, and a cross-shard atomic
    /// would violate MUST #3.
    #[cfg(test)]
    fn flush_syscall_count(&self) -> u64 {
        self.flush_syscalls.get()
    }

    /// Number of flush operations accepted on this shard, one per caller.
    #[cfg(test)]
    fn flush_request_count(&self) -> u64 {
        self.flush_requests.get()
    }

    /// Enqueue one flush request, coalescing it with an open group for the same
    /// inode when that group has not been handed to the pool yet.
    fn enqueue_flush(
        &self,
        token: IoToken,
        mode: FlushMode,
        lease: DescriptorLease,
    ) -> io::Result<()> {
        let identity = flush_identity(lease.as_raw_fd())?;
        // SAFETY: flush_groups is shard-local and only mutated by the owner
        // thread inside submit().
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let groups = unsafe { &mut *self.flush_groups.get() };
        self.flush_requests.set(self.flush_requests.get() + 1);
        match groups.iter_mut().find(|group| group.identity == identity) {
            // Merge into the open group. `All` sorts above `Data`, so the
            // group upgrades to the strongest guarantee any waiter needs.
            Some(group) => {
                group.mode = group.mode.max(mode);
                group.waiters.push((token, mode));
            }
            None => groups.push(FlushGroup {
                identity,
                mode,
                lease,
                waiters: vec![(token, mode)],
            }),
        }
        Ok(())
    }

    /// Close every open flush group and hand each one to the file thread pool.
    ///
    /// Called from the two owner-thread event-loop boundaries (`drain` and
    /// `park`) so that all flushes requested by tasks polled since the last
    /// boundary share one syscall, while a request made after a group was
    /// dispatched can never join it. Groups are removed before the closure is
    /// queued, so a group is never in flight and open at the same time.
    fn dispatch_flush_groups(&self) {
        // SAFETY: shard-local, owner-thread-only list.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        if unsafe { &*self.flush_groups.get() }.is_empty() {
            return;
        }
        // SAFETY: flush_groups is shard-local; this runs on the owner thread
        // from drain/park, never re-entrantly from submit.
        let pending = {
            // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
            let groups = unsafe { &mut *self.flush_groups.get() };
            mem::take(groups)
        };
        for group in pending {
            let FlushGroup {
                mode,
                lease,
                waiters,
                ..
            } = group;
            let completed_tx = self.completed_tx.clone();
            let thread_pool = self.thread_pool.clone();
            // The syscall runs once for the whole group; every waiter is
            // published only after it returned, so no caller can observe
            // durability for a flush that has not happened yet.
            self.flush_syscalls.set(self.flush_syscalls.get() + 1);
            thread_pool.execute(move || {
                let fd = lease.as_raw_fd();
                let outcome = match SafeFdFile::new(fd) {
                    Ok(mut file) => {
                        let file = file.as_mut();
                        match mode {
                            FlushMode::Data => file.sync_data(),
                            FlushMode::All => file.sync_all(),
                        }
                    }
                    Err(error) => Err(error),
                };
                // `io::Error` is not `Clone`, so the failure is carried as a
                // raw code and rebuilt per waiter.
                let failure = outcome.as_ref().err().map(|error| error.raw_os_error());
                for (token, waiter_mode) in waiters {
                    let completion = match failure {
                        None => Ok(waiter_mode.completion()),
                        Some(Some(code)) => Err(IoError::Io(io::Error::from_raw_os_error(code))),
                        Some(None) => Err(IoError::Io(io::Error::other("file flush failed"))),
                    };
                    let _ = completed_tx.send((token, completion));
                }
            });
        }
    }
}

impl IoProvider for MioBackend {
    type Completion = (IoToken, Result<CompletionKind, IoError>);

    fn submit(&self, op: Op) -> Result<IoToken, IoError> {
        let io_token = IoToken::next_from(&self.next_io_token);

        // SAFETY: We have exclusive, single-threaded access.
        let next_token = unsafe { &mut *self.next_token.get() };
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let poll = unsafe { &mut *self.poll.get() };
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let pending_ops = unsafe { &mut *self.pending_ops.get() };
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let descriptor_leases = unsafe { &mut *self.descriptor_leases.get() };
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let descriptor_lease_tokens = unsafe { &mut *self.descriptor_lease_tokens.get() };

        let mio_token = Token(*next_token);
        *next_token += 1;
        let slot = mio_token.0;
        Self::ensure_slot_capacity(pending_ops, slot);
        Self::ensure_slot_capacity(descriptor_leases, slot);
        Self::ensure_slot_capacity(descriptor_lease_tokens, slot);
        // SAFETY: udp_recv_buffers is shard-local and only accessed from this
        // backend owner thread while submitting operations.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        Self::ensure_slot_capacity(unsafe { &mut *self.udp_recv_buffers.get() }, slot);

        // Acquire the lease before registration/queueing.  File operations
        // move it into the worker closure; readiness operations retain it in
        // the slot until their terminal completion.
        let descriptor_lease = if matches!(op, Op::Nop | Op::Close { .. }) {
            None
        } else {
            match DescriptorLease::acquire(op.as_raw_fd()) {
                Ok(lease) => Some(lease),
                Err(error) => {
                    return Err(IoError::Io(error));
                }
            }
        };
        descriptor_leases[slot] = descriptor_lease;
        descriptor_lease_tokens[slot] = Some(io_token);
        let mut registration_error = None;
        let leased_fd = descriptor_leases[slot]
            .as_ref()
            .map(DescriptorLease::as_raw_fd);
        match op {
            Op::UdpRecv { fd, mut buffer } => {
                // Register the UDP socket for read events.
                let effective_fd = leased_fd.unwrap_or(fd);
                let mut source = mio::unix::SourceFd(&effective_fd);
                if let Err(e) =
                    poll.registry()
                        .register(&mut source, mio_token, mio::Interest::READABLE)
                {
                    registration_error = Some(e);
                }
                // SAFETY: udp_recv_buffers is shard-local and only touched by
                // the owning runtime thread while submitting/draining.
                // `recv_from` takes a safe `&mut [u8]`, so it must never see
                // the uninitialized spare capacity supplied by rent buffers.
                buffer.prepare_for_safe_read();
                // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                let udp_recv_buffers = unsafe { &mut *self.udp_recv_buffers.get() };
                udp_recv_buffers[slot] = Some(buffer);
                pending_ops[slot] = Some((
                    io_token,
                    Op::UdpRecv {
                        fd,
                        buffer: Buffer::empty(),
                    },
                ));
            }
            op => match &op {
                Op::Accept { fd } => {
                    let effective_fd = leased_fd.unwrap_or(*fd);
                    let mut source = mio::unix::SourceFd(&effective_fd);
                    if let Err(e) =
                        poll.registry()
                            .register(&mut source, mio_token, Interest::READABLE)
                    {
                        registration_error = Some(e);
                    } else {
                        pending_ops[slot] = Some((io_token, op));
                    }
                }
                // `ReadRent` differs from `Read` only in who owns the buffer:
                // rent keeps the caller's allocation instead of allocating one
                // at completion time. The readiness registration is identical,
                // which is why the two share one arm.
                Op::Read { fd, .. } | Op::ReadRent { fd, .. } => {
                    let effective_fd = leased_fd.unwrap_or(*fd);
                    let mut source = mio::unix::SourceFd(&effective_fd);
                    if let Err(e) =
                        poll.registry()
                            .register(&mut source, mio_token, Interest::READABLE)
                    {
                        registration_error = Some(e);
                    } else {
                        pending_ops[slot] = Some((io_token, op));
                    }
                }
                Op::Write { fd, .. } | Op::WriteStatic { fd, .. } | Op::WriteRent { fd, .. } => {
                    let effective_fd = leased_fd.unwrap_or(*fd);
                    let mut source = mio::unix::SourceFd(&effective_fd);
                    if let Err(e) =
                        poll.registry()
                            .register(&mut source, mio_token, Interest::WRITABLE)
                    {
                        registration_error = Some(e);
                    } else {
                        pending_ops[slot] = Some((io_token, op));
                    }
                }
                Op::ReadVectored { fd, .. } => {
                    let effective_fd = leased_fd.unwrap_or(*fd);
                    let mut source = mio::unix::SourceFd(&effective_fd);
                    if let Err(e) =
                        poll.registry()
                            .register(&mut source, mio_token, Interest::READABLE)
                    {
                        registration_error = Some(e);
                    } else {
                        pending_ops[slot] = Some((io_token, op));
                    }
                }
                Op::WriteVectored { fd, .. } => {
                    let effective_fd = leased_fd.unwrap_or(*fd);
                    let mut source = mio::unix::SourceFd(&effective_fd);
                    if let Err(e) =
                        poll.registry()
                            .register(&mut source, mio_token, Interest::WRITABLE)
                    {
                        registration_error = Some(e);
                    } else {
                        pending_ops[slot] = Some((io_token, op));
                    }
                }
                Op::Connect { fd, addr: _ } => {
                    let effective_fd = leased_fd.unwrap_or(*fd);
                    let mut source = mio::unix::SourceFd(&effective_fd);
                    if let Err(e) =
                        poll.registry()
                            .register(&mut source, mio_token, Interest::WRITABLE)
                    {
                        registration_error = Some(e);
                    } else {
                        pending_ops[slot] = Some((io_token, op));
                    }
                }
                // File operations are handled asynchronously in thread pool
                Op::ReadFile { fd, offset, len } => {
                    let completed_tx = self.completed_tx.clone();
                    let thread_pool = self.thread_pool.clone();
                    let io_token_copy = io_token;
                    let fd_copy = leased_fd.unwrap_or(*fd);
                    let offset_copy = *offset;
                    let len_copy = *len;

                    thread_pool.execute(move || {
                        let result = match SafeFdFile::new(fd_copy) {
                            Ok(mut safe_file) => safe_file
                                .as_mut()
                                .seek(SeekFrom::Start(offset_copy))
                                .and_then(|_| {
                                    let mut buf = Buffer::new_zeroed(len_copy);
                                    safe_file
                                        .as_mut()
                                        .read(buf.as_mut())
                                        .and_then(|bytes_read| {
                                            if buf.publish_read_len(bytes_read) {
                                                Ok(CompletionKind::ReadFile {
                                                    bytes_read,
                                                    data: buf,
                                                })
                                            } else {
                                                Err(io::Error::new(
                                                    io::ErrorKind::InvalidData,
                                                    "file read exceeded buffer capacity",
                                                ))
                                            }
                                        })
                                })
                                .map_err(IoError::Io),
                            Err(err) => Err(IoError::Io(err)),
                        };

                        let _ = completed_tx.send((io_token_copy, result));
                    });
                }
                Op::WriteFile { fd, offset, data } => {
                    let completed_tx = self.completed_tx.clone();
                    let thread_pool = self.thread_pool.clone();
                    let io_token_copy = io_token;
                    let fd_copy = leased_fd.unwrap_or(*fd);
                    let offset_copy = *offset;
                    let data_clone = data.clone();

                    thread_pool.execute(move || {
                        let result = match SafeFdFile::new(fd_copy) {
                            Ok(mut safe_file) => safe_file
                                .as_mut()
                                .seek(SeekFrom::Start(offset_copy))
                                .and_then(|_| {
                                    safe_file.as_mut().write(data_clone.as_ref()).map(
                                        |bytes_written| CompletionKind::WriteFile { bytes_written },
                                    )
                                })
                                .map_err(IoError::Io),
                            Err(err) => Err(IoError::Io(err)),
                        };

                        let _ = completed_tx.send((io_token_copy, result));
                    });
                }
                Op::UdpSend {
                    fd,
                    data: _data,
                    addr: _addr,
                } => {
                    // Register the UDP socket for write events, make it asynchronous
                    let effective_fd = leased_fd.unwrap_or(*fd);
                    let mut source = mio::unix::SourceFd(&effective_fd);
                    if let Err(e) =
                        poll.registry()
                            .register(&mut source, mio_token, mio::Interest::WRITABLE)
                    {
                        eprintln!("Failed to register UDP socket with mio for send: {e}");
                    }
                    // Store the data buffer and address with the op. The op contains the Buffer.
                    pending_ops[slot] = Some((io_token, op));
                }
                // A flush is never issued here. `fsync(2)`/`fdatasync(2)` can
                // block for milliseconds, and running either inline would stall
                // the shard's whole event loop (AGENTS.md MUST NOT #7). The
                // request joins this shard's group-commit queue and is handed to
                // the same thread pool that already carries file reads and
                // writes at the next event-loop boundary.
                Op::Fsync { .. } | Op::Fdatasync { .. } => {
                    let mode = if matches!(op, Op::Fdatasync { .. }) {
                        FlushMode::Data
                    } else {
                        FlushMode::All
                    };
                    // The group owns the acceptance lease outright: it must
                    // outlive every waiter's slot until the syscall returns.
                    match descriptor_leases[slot].take() {
                        Some(lease) => {
                            if let Err(error) = self.enqueue_flush(io_token, mode, lease) {
                                registration_error = Some(error);
                            }
                        }
                        None => {
                            registration_error = Some(io::Error::other(
                                "flush submission lost its descriptor lease",
                            ));
                        }
                    }
                }
                Op::Close { fd } => {
                    let result = {
                        // SAFETY: Close consumes the raw fd for this operation;
                        // callers must not use it after submitting Op::Close.
                        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                        let res = unsafe { libc::close(*fd) };
                        if res == -1 {
                            Err(IoError::Io(io::Error::last_os_error()))
                        } else {
                            Ok(CompletionKind::Close)
                        }
                    };
                    self.complete_local((io_token, result));
                    descriptor_leases[slot] = None;
                    descriptor_lease_tokens[slot] = None;
                }
                Op::PollAdd {
                    fd,
                    events,
                    multishot: _,
                } => {
                    // Register fd with mio for readiness polling
                    let effective_fd = leased_fd.unwrap_or(*fd);
                    let mut source = mio::unix::SourceFd(&effective_fd);
                    let events_u32 = *events;
                    let interest = if events_u32 & libc::POLLIN as u32 != 0 {
                        Interest::READABLE
                    } else if events_u32 & libc::POLLOUT as u32 != 0 {
                        Interest::WRITABLE
                    } else {
                        Interest::READABLE | Interest::WRITABLE
                    };
                    if let Err(e) = poll.registry().register(&mut source, mio_token, interest) {
                        registration_error = Some(e);
                    } else {
                        pending_ops[slot] = Some((io_token, op));
                    }
                }
                _ => {
                    registration_error = Some(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Operation not supported by the mio readiness backend",
                    ));
                    descriptor_leases[slot] = None;
                    descriptor_lease_tokens[slot] = None;
                }
            },
        };

        // Registration failures and unsupported/immediate operations do not
        // leave a pending slot.  Release their acceptance lease here; file
        // jobs retain theirs until the completion token is drained.
        if let Some(error) = registration_error {
            pending_ops[slot] = None;
            // A failed UDP registration may have staged its buffer before the
            // error was observed; discard that staged state with the lease.
            // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
            unsafe {
                (&mut *self.udp_recv_buffers.get())[slot] = None;
            }
            descriptor_leases[slot] = None;
            descriptor_lease_tokens[slot] = None;
            return Err(IoError::Io(error));
        }

        Ok(io_token)
    }

    fn pending_operation_count(&self) -> usize {
        // SAFETY: queried only by the owning fallback event-loop thread.
        let pending = unsafe { &*self.pending_ops.get() }
            .iter()
            .filter(|entry| entry.is_some())
            .count();
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let udp = unsafe { &*self.udp_recv_buffers.get() }
            .iter()
            .filter(|entry| entry.is_some())
            .count();
        // Flush requests waiting for the next event-loop boundary to dispatch
        // their group are accepted work the owner still owes a completion.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let flushes = unsafe { &*self.flush_groups.get() }
            .iter()
            .map(|group| group.waiters.len())
            .sum::<usize>();
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let completed = unsafe { &*self.completed_local.get() }.len();
        pending + udp + flushes + completed
    }

    fn retained_descriptor_count(&self) -> usize {
        // SAFETY: queried only by the owning fallback event-loop thread at
        // termination, while all backend storage is owner-thread exclusive.
        let leases = unsafe { &*self.descriptor_leases.get() }
            .iter()
            .filter(|lease| lease.is_some())
            .count();
        // Accepted descriptors may be held by a completion until the owner
        // consumes it; count those independently from operation leases.
        // SAFETY: the completion vector is owner-thread exclusive here.
        let accepted = unsafe { &*self.completed_local.get() }
            .iter()
            .filter(|(_, result)| matches!(result, Ok(CompletionKind::Accept { .. })))
            .count();
        // An undispatched flush group holds the lease its syscall will run
        // against, moved out of the waiter slot that accepted it. Descriptors
        // already handed to the thread pool are not counted: that in-flight
        // window was never included for file reads and writes either.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let flush_groups = unsafe { &*self.flush_groups.get() }.len();
        leases + accepted + flush_groups
    }

    fn cancel(&self, token: IoToken) -> bool {
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let poll = unsafe { &mut *self.poll.get() };
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let pending_ops = unsafe { &mut *self.pending_ops.get() };
        let Some((slot, entry)) = pending_ops
            .iter_mut()
            .enumerate()
            .find(|(_, entry)| entry.as_ref().is_some_and(|(id, _)| *id == token))
        else {
            return false;
        };
        let op_fd = entry.as_ref().map(|(_, op)| op.as_raw_fd());
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let fd = unsafe { &*self.descriptor_leases.get() }[slot]
            .as_ref()
            .map(DescriptorLease::as_raw_fd)
            .or(op_fd)
            .unwrap_or(-1);
        let mut source = mio::unix::SourceFd(&fd);
        let _ = poll.registry().deregister(&mut source);
        pending_ops[slot] = None;
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        unsafe {
            (&mut *self.udp_recv_buffers.get())[slot] = None;
            (&mut *self.descriptor_leases.get())[slot] = None;
            (&mut *self.descriptor_lease_tokens.get())[slot] = None;
        }
        true
    }

    /// Block until a readiness notification arrives or `timeout` elapses.
    ///
    /// Returns the number of completions available after the wait: the events
    /// harvested here plus anything the file thread pool produced. A return of
    /// 0 means the timeout fired with nothing ready.
    ///
    /// This is the missing piece for macOS: the default `IoProvider::park` is
    /// a no-op returning 0, so a `MioBackend` worker with no ready work spun
    /// at 100% CPU instead of sleeping. `mio::Poll::poll` is exactly the
    /// blocking primitive needed here, and on both `epoll` and `kqueue` it
    /// parks the thread in the kernel.
    ///
    /// Harvested events are left in the shared `events` buffer and flagged via
    /// `park_left_events`, because the runtime's next call is the drain.
    fn park(&self, timeout: Duration) -> usize {
        // Event-loop boundary: close every open flush group so its syscall is
        // queued before this thread blocks. Without this, a shard whose only
        // outstanding work is a flush would sleep until the pool happened to
        // wake it.
        self.dispatch_flush_groups();
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let poll = unsafe { &mut *self.poll.get() };
        // SAFETY: events is shard-local and reused across park calls; poll()
        // fully re-initializes it each time.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let events = unsafe { &mut *self.events.get() };
        // SAFETY: completed_local is shard-local and only touched by this
        // backend owner thread; park() runs on that thread.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let completed = unsafe { &*self.completed_local.get() }.len();

        // Nothing to wait for if the pool already produced work.
        if completed != 0 {
            self.park_left_events.set(false);
            return completed;
        }

        // A zero timeout would busy-loop; mio treats `None` as "block forever".
        // Use a 1ms floor so the worker still re-runs its budgeted work at a
        // bounded rate while parking the thread between turns.
        let wait = timeout.max(Duration::from_millis(1));
        // `Poll::poll` in mio 1.0 returns `io::Result<()>`, so the event count
        // is read back from the shared buffer instead of from the return value.
        match poll.poll(events, Some(wait)) {
            Ok(()) => {
                // A count of 1, not the true event count: the caller only asks
                // "is there work", and a fabricated count would corrupt
                // `park_completions`.
                let readiness = if events.is_empty() { 0 } else { 1 };
                self.park_left_events.set(readiness != 0);
                readiness + completed
            }
            Err(error) => {
                eprintln!("mio park poll error: {error}");
                self.park_left_events.set(false);
                0
            }
        }
    }

    fn cancel_all(&self) {
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let tokens = unsafe { &*self.descriptor_lease_tokens.get() }
            .iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        for token in tokens {
            let _ = self.cancel(token);
        }
    }

    fn drain_completions(&self) -> TaskPoll<Vec<Self::Completion>> {
        // Event-loop boundary: this is where a shard decides the set of flushes
        // that share one syscall. Everything requested since the previous
        // boundary is closed here and handed to the file thread pool.
        self.dispatch_flush_groups();
        // SAFETY: We have exclusive, single-threaded access.
        let poll = unsafe { &mut *self.poll.get() };
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let events = unsafe { &mut *self.events.get() };
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let pending_ops = unsafe { &mut *self.pending_ops.get() };
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let descriptor_leases = unsafe { &mut *self.descriptor_leases.get() };
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let descriptor_lease_tokens = unsafe { &mut *self.descriptor_lease_tokens.get() };
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let completed_local = unsafe { &mut *self.completed_local.get() };
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let completed_rx = unsafe { &mut *self.completed_rx.get() };

        let mut completions = mem::take(completed_local);

        // First, collect any completed operations from the thread pool
        loop {
            match completed_rx.try_recv() {
                Ok(completion) => {
                    if let Some(slot) = descriptor_lease_tokens
                        .iter()
                        .position(|token| *token == Some(completion.0))
                    {
                        descriptor_lease_tokens[slot] = None;
                        descriptor_leases[slot] = None;
                    }
                    completions.push(completion)
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        // Poll for events non-blockingly to avoid delays — unless park()
        // already harvested them, in which case re-polling would clear the
        // buffer and lose them.
        let park_delivered = self.park_left_events.replace(false);
        if !park_delivered {
            match poll.poll(events, Some(Duration::from_millis(0))) {
                Ok(_) => {}
                Err(e) => {
                    // An error from poll is serious, but we'll just log it for now.
                    eprintln!("mio poll error: {e}");
                    return if completions.is_empty() {
                        TaskPoll::Ready(Vec::new())
                    } else {
                        TaskPoll::Ready(completions)
                    };
                }
            }
        }
        // First, handle any events from mio
        for event in events.iter() {
            let mio_token = event.token();
            let slot = mio_token.0;
            if let Some((io_token, op)) = pending_ops.get_mut(slot).and_then(Option::take) {
                let leased_fd = descriptor_leases
                    .get(slot)
                    .and_then(Option::as_ref)
                    .map(DescriptorLease::as_raw_fd);
                let raw_fd = leased_fd.unwrap_or_else(|| op.as_raw_fd());
                let mut source = SourceFd(&raw_fd);
                let _ = poll.registry().deregister(&mut source);

                // Match by value so the rent arms can move the caller's buffer
                // out and back without a payload copy, and so a WouldBlock
                // requeue reconstructs the op from its own fields instead of
                // deep-cloning the payload (`Buffer::clone` copies bytes).
                let result = match op {
                    Op::Accept { fd } => {
                        let effective_fd = leased_fd.unwrap_or(fd);
                        // The file descriptor is a listening socket, so we can accept a connection.
                        match syscall_accept(effective_fd) {
                            Ok((new_fd, addr)) => Ok(CompletionKind::Accept {
                                // SAFETY: syscall_accept returns a fresh owned
                                // descriptor on success.
                                // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                                fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(new_fd) },
                                addr: Some(addr),
                            }),
                            Err(e) => Err(IoError::Io(e)),
                        }
                    }
                    // Rent read: the buffer is the caller's own allocation, so
                    // the kernel writes straight into it and no second copy is
                    // needed. The buffer's spare capacity is uninitialized;
                    // `read` only writes it, and the length is published only
                    // after the syscall reports success.
                    Op::ReadRent { fd, mut buffer } => {
                        let effective_fd = leased_fd.unwrap_or(fd);
                        let mut requeue = false;
                        // The loop returns a count so `buffer` stays owned past
                        // it; the completion variant consumes the buffer only
                        // after the requeue decision has been made.
                        let bytes = loop {
                            // SAFETY: `as_mut_ptr` + `capacity()` describe exactly
                            // the allocation. libc::read writes at most that many
                            // bytes and never reads them.
                            // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                            let ret = unsafe {
                                libc::read(
                                    effective_fd,
                                    buffer.as_mut_ptr() as *mut libc::c_void,
                                    buffer.capacity(),
                                )
                            };
                            if ret < 0 {
                                let err = io::Error::last_os_error();
                                if err.kind() == io::ErrorKind::Interrupted {
                                    continue;
                                }
                                if err.kind() == io::ErrorKind::WouldBlock {
                                    requeue = true;
                                }
                                break Err(IoError::Io(err));
                            }
                            break Ok(ret as usize);
                        };
                        // Requeue while `buffer` is still owned; only the
                        // non-requeue path is allowed to consume it.
                        if requeue {
                            let mut source = SourceFd(&effective_fd);
                            if let Err(reg_err) =
                                poll.registry()
                                    .register(&mut source, mio_token, Interest::READABLE)
                            {
                                eprintln!(
                                    "Failed to re-register fd for rent read after WouldBlock: {reg_err}"
                                );
                            }
                            pending_ops[slot] = Some((io_token, Op::ReadRent { fd, buffer }));
                            continue;
                        }

                        match bytes {
                            Ok(bytes_read) => match publish_read_len(&mut buffer, bytes_read) {
                                Ok(()) => Ok(CompletionKind::ReadRent { buffer }),
                                Err(error) => Err(error),
                            },
                            Err(error) => Err(error),
                        }
                    }
                    Op::Read { fd, offset, len } => {
                        let effective_fd = leased_fd.unwrap_or(fd);
                        let mut buffer = Buffer::with_capacity(len);
                        let mut requeue = false;
                        let result = loop {
                            // SAFETY: buffer has at least `len` capacity and
                            // libc::read writes at most that many bytes.
                            // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                            let ret = unsafe {
                                libc::read(
                                    effective_fd,
                                    buffer.as_mut_ptr() as *mut libc::c_void,
                                    len,
                                )
                            };
                            if ret < 0 {
                                let err = io::Error::last_os_error();
                                if err.kind() == io::ErrorKind::Interrupted {
                                    continue;
                                }
                                if err.kind() == io::ErrorKind::WouldBlock {
                                    requeue = true;
                                }
                                break Err(IoError::Io(err));
                            } else {
                                let bytes_read = ret as usize;
                                match publish_read_len(&mut buffer, bytes_read) {
                                    Ok(()) => {
                                        break Ok(CompletionKind::Read {
                                            bytes_read,
                                            data: buffer,
                                        })
                                    }
                                    Err(error) => break Err(error),
                                }
                            }
                        };

                        if requeue {
                            let mut source = SourceFd(&effective_fd);
                            if let Err(reg_err) =
                                poll.registry()
                                    .register(&mut source, mio_token, Interest::READABLE)
                            {
                                eprintln!(
                                    "Failed to re-register fd for read after WouldBlock: {reg_err}"
                                );
                            }
                            // Reconstruct from the arm's own fields. The old
                            // `op.clone()` deep-copied `Buffer` (payload copy)
                            // to re-arm a read that had not produced a byte yet.
                            // `Op::Read` carries only a length, so re-arming it
                            // is free. The old `op.clone()` still walked the
                            // `Buffer` clone path for the other variants.
                            pending_ops[slot] = Some((io_token, Op::Read { fd, offset, len }));
                            continue;
                        }

                        result
                    }
                    // Rent write: same partial-write loop as `Write`, but the
                    // payload is the caller's own buffer and it is returned on
                    // completion instead of being dropped.
                    Op::WriteRent { fd, data } => {
                        let effective_fd = leased_fd.unwrap_or(fd);
                        let mut written = 0usize;
                        let mut requeue = false;
                        // The loop reports progress instead of building the
                        // completion, so `data` stays owned until after the
                        // requeue decision.
                        loop {
                            let remaining = &data[written..];
                            if remaining.is_empty() {
                                break;
                            }
                            // SAFETY: remaining points to initialized bytes owned
                            // by `data`; write reads at most len bytes.
                            // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                            let ret = unsafe {
                                libc::write(
                                    effective_fd,
                                    remaining.as_ptr() as *const libc::c_void,
                                    remaining.len(),
                                )
                            };
                            if ret < 0 {
                                let err = io::Error::last_os_error();
                                if err.kind() == io::ErrorKind::Interrupted {
                                    continue;
                                }
                                if err.kind() == io::ErrorKind::WouldBlock && written == 0 {
                                    requeue = true;
                                    break;
                                }
                                // A partial write followed by an error is a
                                // successful partial write, not a failure of the
                                // whole operation.
                                break;
                            }
                            written += ret as usize;
                            if written == data.len() {
                                break;
                            }
                        }

                        if requeue {
                            let mut source = SourceFd(&effective_fd);
                            if let Err(reg_err) =
                                poll.registry()
                                    .register(&mut source, mio_token, Interest::WRITABLE)
                            {
                                eprintln!(
                                    "Failed to re-register fd for rent write after WouldBlock: {reg_err}"
                                );
                            }
                            pending_ops[slot] = Some((io_token, Op::WriteRent { fd, data }));
                            continue;
                        }

                        Ok(CompletionKind::WriteRent {
                            bytes_written: written,
                            data,
                        })
                    }
                    Op::Write { fd, offset, data } => {
                        let effective_fd = leased_fd.unwrap_or(fd);
                        let mut written = 0usize;
                        let mut requeue = false;
                        let result = loop {
                            let remaining = &data[written..];
                            if remaining.is_empty() {
                                break Ok(CompletionKind::Write {
                                    bytes_written: written,
                                });
                            }
                            // SAFETY: remaining points to initialized bytes
                            // owned by `data`; write reads at most len bytes.
                            // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                            let ret = unsafe {
                                libc::write(
                                    effective_fd,
                                    remaining.as_ptr() as *const libc::c_void,
                                    remaining.len(),
                                )
                            };
                            if ret < 0 {
                                let err = io::Error::last_os_error();
                                if err.kind() == io::ErrorKind::Interrupted {
                                    continue;
                                }
                                if err.kind() == io::ErrorKind::WouldBlock {
                                    if written == 0 {
                                        requeue = true;
                                        break Err(IoError::Io(err));
                                    } else {
                                        break Ok(CompletionKind::Write {
                                            bytes_written: written,
                                        });
                                    }
                                }
                                break if written > 0 {
                                    Ok(CompletionKind::Write {
                                        bytes_written: written,
                                    })
                                } else {
                                    Err(IoError::Io(err))
                                };
                            } else {
                                written += ret as usize;
                                if written == data.len() {
                                    break Ok(CompletionKind::Write {
                                        bytes_written: written,
                                    });
                                }
                            }
                        };

                        if requeue {
                            let mut source = SourceFd(&effective_fd);
                            if let Err(reg_err) =
                                poll.registry()
                                    .register(&mut source, mio_token, Interest::WRITABLE)
                            {
                                eprintln!(
                                    "Failed to re-register fd for write after WouldBlock: {reg_err}"
                                );
                            }
                            // Reconstruct from the arm's own fields instead of
                            // `op.clone()`: `Buffer::clone` copies the whole
                            // payload, so cloning to re-arm a partial write
                            // doubled the bytes on the wire path.
                            pending_ops[slot] = Some((io_token, Op::Write { fd, offset, data }));
                            continue;
                        }

                        result
                    }
                    Op::WriteStatic { fd, data } => {
                        let effective_fd = leased_fd.unwrap_or(fd);
                        let mut written = 0usize;
                        let mut requeue = false;
                        let result = loop {
                            let remaining = &data[written..];
                            if remaining.is_empty() {
                                break Ok(CompletionKind::Write {
                                    bytes_written: written,
                                });
                            }
                            // SAFETY: remaining points to static initialized
                            // bytes; write reads at most len bytes.
                            // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                            let ret = unsafe {
                                libc::write(
                                    effective_fd,
                                    remaining.as_ptr() as *const libc::c_void,
                                    remaining.len(),
                                )
                            };
                            if ret < 0 {
                                let err = io::Error::last_os_error();
                                if err.kind() == io::ErrorKind::Interrupted {
                                    continue;
                                }
                                if err.kind() == io::ErrorKind::WouldBlock {
                                    if written == 0 {
                                        requeue = true;
                                        break Err(IoError::Io(err));
                                    } else {
                                        break Ok(CompletionKind::Write {
                                            bytes_written: written,
                                        });
                                    }
                                }
                                break if written > 0 {
                                    Ok(CompletionKind::Write {
                                        bytes_written: written,
                                    })
                                } else {
                                    Err(IoError::Io(err))
                                };
                            } else {
                                written += ret as usize;
                                if written == data.len() {
                                    break Ok(CompletionKind::Write {
                                        bytes_written: written,
                                    });
                                }
                            }
                        };

                        if requeue {
                            let mut source = SourceFd(&effective_fd);
                            if let Err(reg_err) =
                                poll.registry()
                                    .register(&mut source, mio_token, Interest::WRITABLE)
                            {
                                eprintln!(
                                    "Failed to re-register fd for static write after WouldBlock: {reg_err}"
                                );
                            }
                            pending_ops[slot] = Some((io_token, Op::WriteStatic { fd, data }));
                            continue;
                        }

                        result
                    }
                    Op::ReadFile { fd, offset, len } => {
                        // This path should ideally not be hit as these are handled sync in submit
                        // but included for completeness if event-driven file I/O were enabled.
                        // SAFETY: File temporarily wraps fd for std APIs, then
                        // into_raw_fd prevents closing so ownership stays outside.
                        let effective_fd = leased_fd.unwrap_or(fd);
                        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                        let mut file = unsafe { std::fs::File::from_raw_fd(effective_fd) };
                        let result = file.seek(SeekFrom::Start(offset)).and_then(|_| {
                            let mut buf = Buffer::new_zeroed(len);
                            file.read(buf.as_mut()).and_then(|bytes_read| {
                                if buf.publish_read_len(bytes_read) {
                                    Ok(CompletionKind::ReadFile {
                                        bytes_read,
                                        data: buf,
                                    })
                                } else {
                                    Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "file read exceeded buffer capacity",
                                    ))
                                }
                            })
                        });
                        let _ = file.into_raw_fd();
                        result.map_err(IoError::Io)
                    }
                    Op::WriteFile { fd, offset, data } => {
                        // This path should ideally not be hit.
                        // SAFETY: File temporarily wraps fd for std APIs, then
                        // into_raw_fd prevents closing so ownership stays outside.
                        let effective_fd = leased_fd.unwrap_or(fd);
                        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                        let mut file = unsafe { std::fs::File::from_raw_fd(effective_fd) };
                        let result = file
                            .seek(SeekFrom::Start(offset))
                            .and_then(|_| file.write(data.as_ref()))
                            .map(|bytes_written| CompletionKind::WriteFile { bytes_written });
                        let _ = file.into_raw_fd();
                        result.map_err(IoError::Io)
                    }
                    Op::UdpRecv { fd, .. } => {
                        let effective_fd = leased_fd.unwrap_or(fd);
                        // Matched by `event.token()`
                        // SAFETY: udp_recv_buffers is shard-local and only
                        // accessed by this backend owner thread.
                        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                        let udp_recv_buffers = unsafe { &mut *self.udp_recv_buffers.get() };
                        if let Some(mut buffer) =
                            udp_recv_buffers.get_mut(slot).and_then(Option::take)
                        {
                            // SAFETY: UdpSocket temporarily wraps fd and is
                            // converted back with into_raw_fd before drop.
                            // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                            let socket = unsafe { std::net::UdpSocket::from_raw_fd(effective_fd) };
                            let mut retry = false;
                            let result = match socket.recv_from(buffer.as_mut()) {
                                Ok((bytes_read, addr)) => {
                                    match publish_read_len(&mut buffer, bytes_read) {
                                        Ok(()) => Ok(CompletionKind::UdpRecv {
                                            bytes_read,
                                            buffer,
                                            addr,
                                        }),
                                        Err(error) => Err(error),
                                    }
                                }
                                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                    let mut mio_source = SourceFd(&effective_fd);
                                    if let Err(reg_err) = poll.registry().register(
                                        &mut mio_source,
                                        mio_token,
                                        mio::Interest::READABLE,
                                    ) {
                                        eprintln!(
                                            "Failed to re-register UDP socket with mio: {reg_err}"
                                        );
                                    }
                                    udp_recv_buffers[slot] = Some(buffer);
                                    pending_ops[slot] = Some((
                                        io_token,
                                        Op::UdpRecv {
                                            fd,
                                            buffer: Buffer::empty(),
                                        },
                                    ));
                                    retry = true;
                                    Err(IoError::Io(e))
                                }
                                Err(e) => Err(IoError::Io(e)),
                            };
                            let _ = socket.into_raw_fd();
                            if retry {
                                continue;
                            }
                            result
                        } else {
                            Err(IoError::Other(
                                "UDP receive buffer not found for event".to_string(),
                            ))
                        }
                    }
                    Op::UdpSend { fd, data, addr } => {
                        // Perform UDP send operation

                        // SAFETY: UdpSocket temporarily wraps fd and is
                        // converted back with into_raw_fd before drop.
                        let effective_fd = leased_fd.unwrap_or(fd);
                        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                        let socket = unsafe { std::net::UdpSocket::from_raw_fd(effective_fd) };
                        let result = socket.send_to(data.as_ref(), addr).map(|bytes_written| {
                            CompletionKind::UdpSend {
                                bytes_written,
                                data: data.clone(),
                            }
                        });
                        let _ = socket.into_raw_fd();
                        result.map_err(IoError::Io)
                    }
                    Op::Connect { fd, addr: _ } => {
                        let raw_fd = leased_fd.unwrap_or(fd);
                        // SAFETY: TcpStream temporarily wraps fd to call
                        // take_error, then into_raw_fd preserves ownership.
                        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                        let sock = unsafe { std::net::TcpStream::from_raw_fd(raw_fd) };
                        let result = match sock.take_error() {
                            Ok(None) => Ok(CompletionKind::Connect),
                            Ok(Some(e)) => Err(IoError::Io(e)),
                            Err(e) => Err(IoError::Io(e)),
                        };
                        let _ = sock.into_raw_fd();
                        result
                    }
                    Op::ReadVectored { fd, buffers } => {
                        let effective_fd = leased_fd.unwrap_or(fd);
                        // `Op::ReadVectored` carries placeholder buffers whose
                        // only meaningful property is capacity; the real read
                        // destinations are built from that capacity. Renamed so
                        // the arm's own `buffers` stays owned for the requeue
                        // path (shadowing the same name trips the loop move
                        // analysis).
                        let mut dest_buffers: smallvec::SmallVec<[Buffer; 4]> = buffers
                            .iter()
                            .map(|buffer| Buffer::new_uninitialized(buffer.capacity()))
                            .collect();
                        let iovecs: Vec<libc::iovec> = dest_buffers
                            .iter_mut()
                            .map(|buf| libc::iovec {
                                iov_base: buf.as_mut_ptr() as *mut libc::c_void,
                                iov_len: buf.capacity(),
                            })
                            .collect();
                        let mut requeue = false;
                        let result = loop {
                            // SAFETY: iovecs point to writable capacity in
                            // dest_buffers and remain valid for this call.
                            // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                            let ret = unsafe {
                                libc::readv(effective_fd, iovecs.as_ptr(), iovecs.len() as i32)
                            };
                            if ret < 0 {
                                let err = io::Error::last_os_error();
                                if err.kind() == io::ErrorKind::Interrupted {
                                    continue;
                                }
                                if err.kind() == io::ErrorKind::WouldBlock {
                                    requeue = true;
                                }
                                break Err(IoError::Io(err));
                            } else if ret == 0 {
                                break Ok(CompletionKind::ReadVectored {
                                    bytes_read: 0,
                                    buffers: dest_buffers,
                                });
                            } else {
                                let bytes_read = ret as usize;
                                let requested_capacity: usize =
                                    iovecs.iter().map(|iov| iov.iov_len).sum();
                                if bytes_read > requested_capacity {
                                    break Err(IoError::Other(format!(
                                        "readv reported {bytes_read} bytes for {requested_capacity} byte buffers"
                                    )));
                                }
                                let mut remaining = bytes_read;
                                for buf in dest_buffers.iter_mut() {
                                    let can_read = std::cmp::min(remaining, buf.capacity());
                                    // `can_read` is bounded by both the checked
                                    // readv result and this buffer's capacity.
                                    assert!(buf.publish_read_len(can_read));
                                    remaining -= can_read;
                                    if remaining == 0 {
                                        break;
                                    }
                                }
                                break Ok(CompletionKind::ReadVectored {
                                    bytes_read,
                                    buffers: dest_buffers,
                                });
                            }
                        };

                        if requeue {
                            let mut source = SourceFd(&effective_fd);
                            if let Err(reg_err) =
                                poll.registry()
                                    .register(&mut source, mio_token, Interest::READABLE)
                            {
                                eprintln!(
                                    "Failed to re-register fd for readv after WouldBlock: {reg_err}"
                                );
                            }
                            // Reconstruct from the op's own fields. `Buffer::clone`
                            // copies bytes, so cloning to re-arm a read that had
                            // produced nothing duplicated the payload for nothing.
                            pending_ops[slot] = Some((io_token, Op::ReadVectored { fd, buffers }));
                            continue;
                        }

                        result
                    }
                    Op::WriteVectored { fd, buffers } => {
                        let effective_fd = leased_fd.unwrap_or(fd);
                        let total_len: usize = buffers.iter().map(|b| b.len()).sum();
                        if total_len == 0 {
                            Ok(CompletionKind::WriteVectored {
                                bytes_written: 0,
                                buffers: buffers.clone(),
                            })
                        } else {
                            let mut written = 0usize;
                            let mut requeue = false;
                            let result = loop {
                                let iovecs: Vec<libc::iovec> = buffers
                                    .iter()
                                    .enumerate()
                                    .filter_map(|(i, buf)| {
                                        let buf_start: usize =
                                            buffers[..i].iter().map(|b| b.len()).sum();
                                        if written >= buf_start + buf.len() {
                                            return None;
                                        }
                                        let offset = written.saturating_sub(buf_start);
                                        let remaining = buf.len() - offset;
                                        Some(libc::iovec {
                                            // SAFETY: offset is within this
                                            // buffer due to checks above.
                                            // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                                            iov_base: unsafe {
                                                buf.as_ref().as_ptr().add(offset)
                                                    as *mut libc::c_void
                                            },
                                            iov_len: remaining,
                                        })
                                    })
                                    .collect();
                                if iovecs.is_empty() {
                                    break Ok(CompletionKind::WriteVectored {
                                        bytes_written: written,
                                        buffers: buffers.clone(),
                                    });
                                }
                                // SAFETY: iovecs point to initialized bytes in
                                // buffers and remain valid for this syscall.
                                // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                                let ret = unsafe {
                                    libc::writev(effective_fd, iovecs.as_ptr(), iovecs.len() as i32)
                                };
                                if ret < 0 {
                                    let err = io::Error::last_os_error();
                                    if err.kind() == io::ErrorKind::Interrupted {
                                        continue;
                                    }
                                    if err.kind() == io::ErrorKind::WouldBlock {
                                        if written > 0 {
                                            break Ok(CompletionKind::WriteVectored {
                                                bytes_written: written,
                                                buffers: buffers.clone(),
                                            });
                                        }
                                        requeue = true;
                                        break Err(IoError::Io(err));
                                    }
                                    break if written > 0 {
                                        Ok(CompletionKind::WriteVectored {
                                            bytes_written: written,
                                            buffers: buffers.clone(),
                                        })
                                    } else {
                                        Err(IoError::Io(err))
                                    };
                                } else {
                                    written += ret as usize;
                                    if written >= total_len {
                                        break Ok(CompletionKind::WriteVectored {
                                            bytes_written: written,
                                            buffers: buffers.clone(),
                                        });
                                    }
                                }
                            };

                            if requeue {
                                let mut source = SourceFd(&effective_fd);
                                if let Err(reg_err) = poll.registry().register(
                                    &mut source,
                                    mio_token,
                                    Interest::WRITABLE,
                                ) {
                                    eprintln!(
                                        "Failed to re-register fd for writev after WouldBlock: {reg_err}"
                                    );
                                }
                                pending_ops[slot] =
                                    Some((io_token, Op::WriteVectored { fd, buffers }));
                                continue;
                            }

                            result
                        }
                    }
                    Op::PollAdd { .. } => {
                        let mut readiness = 0u32;
                        if event.is_readable() {
                            readiness |= libc::POLLIN as u32;
                        }
                        if event.is_writable() {
                            readiness |= libc::POLLOUT as u32;
                        }
                        Ok(CompletionKind::PollAdd { events: readiness })
                    }
                    // Fsync, Fdatasync and Close are not readiness operations;
                    // they complete from the thread pool or inline.
                    _ => continue,
                };

                // The lease is retained across every readiness requeue and is
                // released only after this terminal result is published.
                // The lease vectors are backend-owned and remain valid for
                // this owner-thread drain pass.
                // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                unsafe {
                    (&mut *self.descriptor_leases.get())[slot] = None;
                    (&mut *self.descriptor_lease_tokens.get())[slot] = None;
                }
                completions.push((io_token, result));
            }
        }

        if completions.is_empty() {
            TaskPoll::Pending
        } else {
            TaskPoll::Ready(completions)
        }
    }
}

#[cfg(all(test, unix))]
mod descriptor_lease_acceptance_tests {
    use super::MioBackend;
    use crate::io::{IoProvider, Op};
    use smallvec::SmallVec;

    #[test]
    fn accepted_read_keeps_descriptor_lease_after_user_close() {
        let mut fds = [0; 2];
        // SAFETY: fds points at two live descriptor slots sized for a pipe.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let piped = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(piped, 0, "pipe creation failed");
        let backend = MioBackend::new().expect("mio backend");
        let token = backend
            .submit(Op::Read {
                fd: fds[0],
                offset: 0,
                len: 1,
            })
            .expect("pipe read admitted");
        assert!(backend.pending_operation_count() >= 1);
        // SAFETY: the read end is still owned by this test and is closed once.
        unsafe { libc::close(fds[0]) };
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let leases = unsafe { &*backend.descriptor_leases.get() };
        assert!(leases.iter().any(|lease| {
            lease
                .as_ref()
                // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
                .is_some_and(|lease| unsafe { libc::fcntl(lease.as_raw_fd(), libc::F_GETFD) } >= 0)
        }));
        let _ = token;
        // SAFETY: write end remains owned by this test and is closed once.
        unsafe { libc::close(fds[1]) };
    }

    #[test]
    fn unsupported_submit_returns_error_without_completion_token() {
        let mut fds = [0; 2];
        // SAFETY: fds points at two live descriptor slots sized for a pipe.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let piped = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(piped, 0, "pipe creation failed");
        let backend = MioBackend::new().expect("mio backend");
        let result = backend.submit(Op::UdpRecvVectored {
            fd: fds[0],
            iovecs: SmallVec::new(),
        });
        assert!(matches!(result, Err(crate::io::IoError::Io(_))));
        assert!(matches!(
            backend.drain_completions(),
            std::task::Poll::Pending
        ));
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }
}

fn syscall_accept(fd: RawFd) -> io::Result<(RawFd, std::net::SocketAddr)> {
    // SAFETY: sockaddr_storage is a plain C storage type and zeroed storage is
    // valid for accept to fill.
    let mut storage: libc::sockaddr_storage =
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
    let mut len = std::mem::size_of_val(&storage) as libc::socklen_t;
    // SAFETY: storage/len pointers are valid for the duration of accept.
    let new_fd = unsafe { libc::accept(fd, &mut storage as *mut _ as *mut _, &mut len) };

    if new_fd < 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: accept filled storage with a sockaddr matching ss_family.
    let addr = unsafe {
        match storage.ss_family as libc::c_int {
            libc::AF_INET => {
                let sockaddr_in = &*(&storage as *const _ as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sockaddr_in.sin_addr.s_addr));
                let port = u16::from_be(sockaddr_in.sin_port);
                std::net::SocketAddr::V4(std::net::SocketAddrV4::new(ip, port))
            }
            libc::AF_INET6 => {
                let sockaddr_in6 = &*(&storage as *const _ as *const libc::sockaddr_in6);
                let ip = std::net::Ipv6Addr::from(sockaddr_in6.sin6_addr.s6_addr);
                let port = u16::from_be(sockaddr_in6.sin6_port);
                std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                    ip,
                    port,
                    sockaddr_in6.sin6_flowinfo,
                    sockaddr_in6.sin6_scope_id,
                ))
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Unsupported address family",
                ));
            }
        }
    };

    Ok((new_fd, addr))
}

// Helper to get RawFd from Op
impl AsRawFd for Op {
    fn as_raw_fd(&self) -> RawFd {
        match *self {
            Op::Accept { fd } => fd,
            Op::Read { fd, .. } => fd,
            Op::Write { fd, .. } => fd,
            Op::WriteStatic { fd, .. } => fd,
            Op::Fsync { fd, .. } => fd,
            Op::Fdatasync { fd, .. } => fd,
            Op::Close { fd, .. } => fd,
            Op::ReadFile { fd, .. } => fd,
            Op::WriteFile { fd, .. } => fd,
            Op::UdpRecv { fd, .. } => fd,
            Op::UdpSend { fd, .. } => fd,
            Op::UdpRecvVectored { fd, .. } => fd,
            Op::UdpSendVectored { fd, .. } => fd,
            Op::ReadVectored { fd, .. } => fd,
            Op::WriteVectored { fd, .. } => fd,
            Op::Connect { fd, .. } => fd,
            Op::ReadRent { fd, .. } => fd,
            Op::WriteRent { fd, .. } => fd,
            Op::PollAdd { fd, .. } => fd,
            Op::Nop => panic!("Nop has no file descriptor"),
        }
    }
}

#[cfg(test)]
mod locality_contract_tests {
    static_assertions::assert_not_impl_any!(super::MioBackend: Send, Sync);
}

#[cfg(all(test, unix))]
mod park_and_rent_tests {
    use super::MioBackend;
    use crate::buffer::Buffer;
    use crate::io::{CompletionKind, IoProvider, Op};
    use std::time::{Duration, Instant};

    fn set_nonblocking(fd: std::os::unix::io::RawFd) {
        // SAFETY: fd is a valid descriptor owned by this test and fcntl only
        // reads its status flags.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0, "F_GETFL failed");
        // SAFETY: same descriptor, same call shape, only adds O_NONBLOCK to the
        // value just read back.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let set = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        assert_eq!(set, 0, "F_SETFL failed");
    }

    /// `park` must actually park. Before this existed, the default
    /// `IoProvider::park` returned 0 immediately, so an idle worker spun at
    /// 100% CPU instead of blocking in the kernel.
    #[test]
    fn park_blocks_rather_than_returning_immediately() {
        let backend = MioBackend::new().expect("mio backend");

        // An empty pipe with no writer is never readable, so any blocking
        // behaviour must come from the wait, not from an early return.
        let mut fds = [0; 2];
        // SAFETY: fds points at two live descriptor slots sized for a pipe.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let piped = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(piped, 0, "pipe creation failed");
        set_nonblocking(fds[0]);

        let token = backend
            .submit(Op::Read {
                fd: fds[0],
                offset: 0,
                len: 4096,
            })
            .expect("register read readiness");

        let started = Instant::now();
        let n = backend.park(Duration::from_millis(60));
        let elapsed = started.elapsed();

        // The wait must have happened, and no completion may be fabricated.
        assert!(
            elapsed >= Duration::from_millis(40),
            "park returned after {elapsed:?}; it is not blocking"
        );
        assert_eq!(n, 0, "park reported completions for a never-readable fd");

        // A second park must still report nothing and must not have consumed
        // the registration.
        assert_eq!(backend.park(Duration::from_millis(1)), 0);
        assert!(backend.pending_operation_count() >= 1);

        assert!(backend.cancel(token));
        // SAFETY: both pipe ends are owned by this test and not aliased.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    /// park() fills the shared event buffer and drain must consume it. A drain
    /// that re-polls with a zero timeout would clear the buffer and lose the
    /// events park just harvested.
    #[test]
    fn park_hands_harvested_events_to_the_next_drain() {
        let backend = MioBackend::new().expect("mio backend");
        let mut fds = [0; 2];
        // SAFETY: fds points at two live descriptor slots sized for a pipe.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let piped = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(piped, 0, "pipe creation failed");

        // Make the read end readable before parking so the wait returns
        // immediately with an event in hand.
        // SAFETY: b"x" is a live 1-byte static and fds[1] is a valid, writable
        // pipe end for the duration of the call.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let written = unsafe { libc::write(fds[1], b"x".as_ptr().cast(), 1) };
        assert_eq!(written, 1, "pipe write failed");
        set_nonblocking(fds[0]);

        let token = backend
            .submit(Op::Read {
                fd: fds[0],
                offset: 0,
                len: 16,
            })
            .expect("register read readiness");

        let n = backend.park(Duration::from_millis(200));
        assert!(n > 0, "park found no readiness on a readable pipe");

        let drained = backend.drain_completions();
        let completions = match drained {
            std::task::Poll::Ready(c) => c,
            std::task::Poll::Pending => panic!("drain reported pending after park"),
        };
        assert_eq!(completions.len(), 1);
        let (tok, result) = completions.into_iter().next().unwrap();
        assert_eq!(tok, token);
        match result {
            Ok(CompletionKind::Read { bytes_read, data }) => {
                assert_eq!(bytes_read, 1);
                assert_eq!(&data[..], b"x");
            }
            other => panic!("unexpected completion: {other:?}"),
        }

        // SAFETY: both pipe ends are owned by this test and not aliased.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    /// The rent operations were `ErrorKind::Unsupported` on the readiness
    /// backend, which made the zero-copy API Linux-only even on macOS, where
    /// it matters most.
    #[test]
    fn read_rent_round_trips_on_the_readiness_backend() {
        let backend = MioBackend::new().expect("mio backend");
        let mut fds = [0; 2];
        // SAFETY: fds points at two live descriptor slots sized for a pipe.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let piped = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(piped, 0, "pipe creation failed");
        // SAFETY: b"hello" is a live 5-byte static and fds[1] is a valid,
        // writable pipe end for the duration of the call.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let written = unsafe { libc::write(fds[1], b"hello".as_ptr().cast(), 5) };
        assert_eq!(written, 5, "pipe write failed");
        set_nonblocking(fds[0]);

        let token = backend
            .submit(Op::ReadRent {
                fd: fds[0],
                buffer: Buffer::with_capacity(32),
            })
            .expect("register rent read");

        let completions = match backend.drain_completions() {
            std::task::Poll::Ready(c) => c,
            std::task::Poll::Pending => {
                // Not ready in the same tick: park, then drain again.
                assert!(backend.park(Duration::from_millis(200)) > 0);
                match backend.drain_completions() {
                    std::task::Poll::Ready(c) => c,
                    std::task::Poll::Pending => panic!("rent read never completed"),
                }
            }
        };
        let (tok, result) = completions.into_iter().next().unwrap();
        assert_eq!(tok, token);
        match result {
            Ok(CompletionKind::ReadRent { buffer }) => {
                assert_eq!(buffer.len(), 5);
                assert_eq!(&buffer[..], b"hello");
            }
            other => panic!("unexpected completion: {other:?}"),
        }

        // SAFETY: both pipe ends are owned by this test and not aliased.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    #[test]
    fn write_rent_round_trips_on_the_readiness_backend() {
        let backend = MioBackend::new().expect("mio backend");
        let mut fds = [0; 2];
        // SAFETY: fds points at two live descriptor slots sized for a pipe.
        // SAFETY: The owning shard validates lifetime, aliasing, and initialization before this operation.
        let piped = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(piped, 0, "pipe creation failed");
        set_nonblocking(fds[1]);

        let token = backend
            .submit(Op::WriteRent {
                fd: fds[1],
                data: Buffer::from_slice(b"payload"),
            })
            .expect("register rent write");

        let completions = loop {
            match backend.drain_completions() {
                std::task::Poll::Ready(c) if !c.is_empty() => break c,
                std::task::Poll::Ready(_) | std::task::Poll::Pending => {
                    assert!(
                        backend.park(Duration::from_millis(200)) > 0,
                        "rent write never became writable"
                    );
                }
            }
        };
        let (tok, result) = completions.into_iter().next().unwrap();
        assert_eq!(tok, token);
        match result {
            Ok(CompletionKind::WriteRent {
                bytes_written,
                data,
            }) => {
                assert_eq!(bytes_written, 7);
                assert_eq!(&data[..], b"payload");
            }
            other => panic!("unexpected completion: {other:?}"),
        }

        // The bytes actually reached the pipe.
        let mut out = [0u8; 16];
        // SAFETY: out is a live 16-byte value and fds[0] is readable.
        let got = unsafe { libc::read(fds[0], out.as_mut_ptr().cast(), out.len()) };
        assert_eq!(got, 7);
        assert_eq!(&out[..7], b"payload");

        // SAFETY: both pipe ends are owned by this test and not aliased.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    /// `Op::Fsync` used to run `sync_all()` inline in `submit`, stalling the
    /// shard's whole event loop (AGENTS.md MUST NOT #7). It now goes through the
    /// bounded file thread pool. The observable difference: `submit` returns
    /// while the flush is still in flight, and the completion arrives later.
    #[test]
    fn fsync_submits_without_running_inline_on_the_calling_thread() {
        use std::io::Write;
        use std::os::unix::io::AsRawFd;

        let mut temp = std::env::temp_dir();
        temp.push(format!("miniss-fsync-submit-{}.bin", std::process::id()));
        // A file large enough that `fsync` cannot complete in microseconds;
        // otherwise the timing assertion below is not meaningful.
        const CHUNK: usize = 8 * 1024 * 1024;
        {
            let mut file = std::fs::File::create(&temp).expect("create temp file");
            let block = vec![0xA5u8; CHUNK];
            for _ in 0..6 {
                file.write_all(&block).expect("fill temp file");
            }
            file.sync_all().expect("prime sync");
        }

        let backend = MioBackend::new().expect("mio backend");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&temp)
            .expect("reopen temp file");
        let fd = file.as_raw_fd();

        let submitted = Instant::now();
        let token = backend.submit(Op::Fsync { fd }).expect("submit fsync");
        let submit_elapsed = submitted.elapsed();

        assert!(
            submit_elapsed < Duration::from_millis(50),
            "submitting fsync took {submit_elapsed:?}; it is running inline on the caller"
        );

        // The completion must still arrive, through the pool.
        let deadline = Instant::now() + Duration::from_secs(10);
        let completions = loop {
            match backend.drain_completions() {
                std::task::Poll::Ready(batch) if !batch.is_empty() => break batch,
                _ => {
                    assert!(Instant::now() < deadline, "fsync never completed");
                    let _ = backend.park(Duration::from_millis(20));
                }
            }
        };
        let (tok, result) = completions.into_iter().next().unwrap();
        assert_eq!(tok, token);
        assert!(result.is_ok(), "fsync failed: {:?}", result.err());

        drop(file);
        let _ = std::fs::remove_file(&temp);
    }
}

/// Group-commit evidence.
///
/// These tests drive one backend directly so the coalescing decision is
/// observed by counting the syscalls the shard issued, not by timing. The
/// runtime-level path (`AsyncFile::sync_data`) is covered in
/// `tests/async_file_tests.rs`.
#[cfg(all(test, unix))]
mod flush_group_commit_tests {
    use super::MioBackend;
    use crate::io::{CompletionKind, IoProvider, IoToken, Op};
    use std::collections::HashSet;
    use std::os::fd::FromRawFd;
    use std::os::unix::io::AsRawFd;
    use std::task::Poll;
    use std::time::{Duration, Instant};

    /// A temp file that removes itself, so a failed assertion cannot leave
    /// scratch files behind.
    struct TempFile {
        path: std::path::PathBuf,
        file: std::fs::File,
    }

    impl TempFile {
        fn new(tag: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "miniss-flush-group-{}-{tag}.bin",
                std::process::id()
            ));
            let file = std::fs::File::create(&path).expect("create temp file");
            Self { path, file }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Drain until `expected` completions arrive or the deadline expires.
    fn drain_until(
        backend: &MioBackend,
        expected: usize,
    ) -> Vec<(IoToken, Result<CompletionKind, crate::io::IoError>)> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = Vec::new();
        while seen.len() < expected {
            match backend.drain_completions() {
                Poll::Ready(batch) => {
                    for completion in batch {
                        seen.push(completion);
                    }
                }
                Poll::Pending => {
                    assert!(
                        Instant::now() < deadline,
                        "only {} of {expected} flush completions arrived",
                        seen.len()
                    );
                    let _ = backend.park(Duration::from_millis(5));
                }
            }
        }
        seen
    }

    /// `CompletionKind` is neither `Clone` nor `Eq`, so completions are
    /// classified into booleans by the caller-supplied predicate.
    fn completion_is(
        completions: &[(IoToken, Result<CompletionKind, crate::io::IoError>)],
        token: IoToken,
        predicate: impl Fn(&CompletionKind) -> bool,
    ) -> bool {
        completions
            .iter()
            .find(|(seen, _)| *seen == token)
            .map(|(_, result)| match result {
                Ok(kind) => predicate(kind),
                Err(error) => panic!("waiter {token:?} failed: {error:?}"),
            })
            .unwrap_or(false)
    }

    /// The headline property: N flush requests for one file on one shard cost
    /// one syscall, not N.
    #[test]
    fn same_file_flushes_in_one_shard_cost_one_syscall() {
        const REQUESTS: usize = 16;
        let temp = TempFile::new("coalesce");
        let fd = temp.file.as_raw_fd();
        let backend = MioBackend::new().expect("mio backend");

        let tokens: Vec<IoToken> = (0..REQUESTS)
            .map(|_| {
                backend
                    .submit(Op::Fdatasync { fd })
                    .expect("submit fdatasync")
            })
            .collect();

        // Nothing may have been issued yet: the group is still open, which is
        // what lets later requests join it.
        assert_eq!(
            backend.flush_syscall_count(),
            0,
            "a flush syscall ran before the event-loop boundary"
        );

        let completions = drain_until(&backend, REQUESTS);
        assert_eq!(backend.flush_request_count(), REQUESTS as u64);
        assert_eq!(
            backend.flush_syscall_count(),
            1,
            "group commit issued {} syscalls for {REQUESTS} requests",
            backend.flush_syscall_count()
        );

        // Every caller still gets its own completion, with its own kind.
        let tokens: HashSet<IoToken> = tokens.into_iter().collect();
        let seen: HashSet<IoToken> = completions.iter().map(|(token, _)| *token).collect();
        assert_eq!(tokens, seen, "group commit lost or duplicated a waiter");
        for (token, result) in completions {
            assert!(
                matches!(result, Ok(CompletionKind::Fdatasync)),
                "waiter {token:?} got {result:?}"
            );
        }
    }

    /// A caller that needs `fsync(2)` must not be served by the weaker
    /// `fdatasync(2)` that another waiter asked for; the group upgrades.
    #[test]
    fn mixed_flush_strength_upgrades_the_group_to_fsync() {
        let temp = TempFile::new("mixed");
        let fd = temp.file.as_raw_fd();
        let backend = MioBackend::new().expect("mio backend");

        let data_first = backend
            .submit(Op::Fdatasync { fd })
            .expect("submit fdatasync");
        let all_second = backend.submit(Op::Fsync { fd }).expect("submit fsync");
        let data_third = backend
            .submit(Op::Fdatasync { fd })
            .expect("submit second fdatasync");

        let completions = drain_until(&backend, 3);
        assert_eq!(backend.flush_syscall_count(), 1);
        assert!(completion_is(&completions, data_first, |kind| matches!(
            kind,
            CompletionKind::Fdatasync
        )));
        assert!(completion_is(&completions, all_second, |kind| matches!(
            kind,
            CompletionKind::Fsync
        )));
        assert!(completion_is(&completions, data_third, |kind| matches!(
            kind,
            CompletionKind::Fdatasync
        )));
    }

    /// A request made after a group was dispatched cannot join it: the syscall
    /// may already have passed the caller's writes. It must open a new group.
    #[test]
    fn flush_requested_after_dispatch_starts_a_new_group() {
        let temp = TempFile::new("late");
        let fd = temp.file.as_raw_fd();
        let backend = MioBackend::new().expect("mio backend");

        backend.submit(Op::Fdatasync { fd }).expect("submit first");
        // The first drain closes the group and queues its syscall.
        let first = drain_until(&backend, 1);
        assert_eq!(backend.flush_syscall_count(), 1);
        assert!(matches!(first[0].1, Ok(CompletionKind::Fdatasync)));

        backend.submit(Op::Fdatasync { fd }).expect("submit second");
        assert_eq!(
            backend.flush_syscall_count(),
            1,
            "a late request was counted before its group was dispatched"
        );
        let second = drain_until(&backend, 1);
        assert_eq!(backend.flush_syscall_count(), 2);
        assert!(matches!(second[0].1, Ok(CompletionKind::Fdatasync)));
    }

    /// Two files are two groups: identity, not "a flush is a flush".
    #[test]
    fn flushes_of_different_files_are_not_coalesced() {
        let first = TempFile::new("file-a");
        let second = TempFile::new("file-b");
        let backend = MioBackend::new().expect("mio backend");

        backend
            .submit(Op::Fdatasync {
                fd: first.file.as_raw_fd(),
            })
            .expect("submit first file");
        backend
            .submit(Op::Fdatasync {
                fd: second.file.as_raw_fd(),
            })
            .expect("submit second file");

        let completions = drain_until(&backend, 2);
        assert_eq!(backend.flush_syscall_count(), 2);
        for (_, result) in completions {
            assert!(matches!(result, Ok(CompletionKind::Fdatasync)));
        }
    }

    /// Same inode, two descriptors: `fsync(2)` is a property of the inode, so
    /// these are the same group. Proves identity is not the descriptor number.
    #[test]
    fn same_inode_through_two_descriptors_coalesces() {
        let temp = TempFile::new("two-fds");
        let second_fd = {
            // SAFETY: fd is a live open descriptor owned by the temp file and
            // is duplicated exactly once into a test-owned `File`.
            let duplicated = unsafe { libc::dup(temp.file.as_raw_fd()) };
            assert!(duplicated >= 0, "dup failed");
            // SAFETY: ownership of the fresh descriptor moves into the File.
            let file = unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicated) };
            std::fs::File::from(file)
        };
        let backend = MioBackend::new().expect("mio backend");

        backend
            .submit(Op::Fdatasync {
                fd: temp.file.as_raw_fd(),
            })
            .expect("submit via first descriptor");
        backend
            .submit(Op::Fdatasync {
                fd: second_fd.as_raw_fd(),
            })
            .expect("submit via second descriptor");

        let completions = drain_until(&backend, 2);
        assert_eq!(backend.flush_syscall_count(), 1);
        for (_, result) in completions {
            assert!(matches!(result, Ok(CompletionKind::Fdatasync)));
        }
    }

    /// Identity lookup must fail closed. A descriptor number that cannot be
    /// statted must not be queued against a guessed identity — the backend
    /// rejects it instead, and nothing is counted as a request.
    ///
    /// `-1` is used directly rather than a closed descriptor: closing a real
    /// descriptor frees a number that a parallel test thread may reuse before
    /// this one runs, which would make the premise racy.
    #[test]
    fn flush_identity_rejects_a_descriptor_it_cannot_stat() {
        let error = super::flush_identity(-1).expect_err("fstat(-1) must fail");
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }
}
