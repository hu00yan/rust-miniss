#![cfg(any(io_backend = "epoll", all(unix, not(target_os = "linux"))))]

//! An `epoll` (or `kqueue` via `mio`) backend for the I/O subsystem.
//!
//! This implementation is designed for a thread-per-core architecture, serving as a
//! fallback for Unix-like systems where `io-uring` is not available. Each `EpollBackend`
//! instance owns a `mio::Poll` handle and is intended for single-threaded use.
//!
//! Like the `uring` backend, this module uses `UnsafeCell` and `unsafe` trait impls
//! to manage thread-local state within the `IoBackend` trait's `&self` methods.

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
        let dup_fd = unsafe { libc::dup(fd) };
        if dup_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: dup_fd is newly owned by this function and is transferred
        // into File exactly once.
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

/// An `epoll` based `IoBackend` implementation using `mio`.
#[derive(Debug)]
pub struct EpollBackend {
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
    /// Same-thread completed operations.
    completed_local: UnsafeCell<Vec<IoCompletion>>,
    /// Completed operations from the file I/O thread pool.
    completed_tx: SyncSender<IoCompletion>,
    completed_rx: UnsafeCell<Receiver<IoCompletion>>,
}

impl EpollBackend {
    /// Creates a new `EpollBackend`.
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
            completed_local: UnsafeCell::new(Vec::new()),
            completed_tx,
            completed_rx: UnsafeCell::new(completed_rx),
        })
    }

    fn complete_local(&self, completion: IoCompletion) {
        // SAFETY: EpollBackend is driven by one runtime thread; completed_local
        // is shard-local and not accessed concurrently.
        let completed_local = unsafe { &mut *self.completed_local.get() };
        completed_local.push(completion);
    }

    #[inline]
    fn ensure_slot_capacity<T>(vec: &mut Vec<Option<T>>, idx: usize) {
        if idx >= vec.len() {
            vec.resize_with(idx + 1, || None);
        }
    }
}

impl IoProvider for EpollBackend {
    type Completion = (IoToken, Result<CompletionKind, IoError>);

    fn submit(&self, op: Op) -> Result<IoToken, IoError> {
        let io_token = IoToken::next_from(&self.next_io_token);

        // SAFETY: We have exclusive, single-threaded access.
        let next_token = unsafe { &mut *self.next_token.get() };
        let poll = unsafe { &mut *self.poll.get() };
        let pending_ops = unsafe { &mut *self.pending_ops.get() };
        let descriptor_leases = unsafe { &mut *self.descriptor_leases.get() };
        let descriptor_lease_tokens = unsafe { &mut *self.descriptor_lease_tokens.get() };

        let mio_token = Token(*next_token);
        *next_token += 1;
        let slot = mio_token.0;
        Self::ensure_slot_capacity(pending_ops, slot);
        Self::ensure_slot_capacity(descriptor_leases, slot);
        Self::ensure_slot_capacity(descriptor_lease_tokens, slot);
        // SAFETY: udp_recv_buffers is shard-local and only accessed from this
        // epoll backend owner thread while submitting operations.
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
                // the owning epoll runtime thread while submitting/draining.
                // `recv_from` takes a safe `&mut [u8]`, so it must never see
                // the uninitialized spare capacity supplied by rent buffers.
                buffer.prepare_for_safe_read();
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
                Op::Read { fd, .. } => {
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
                Op::Write { fd, .. } | Op::WriteStatic { fd, .. } => {
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
                        eprintln!("Failed to register UDP socket with epoll for send: {e}");
                    }
                    // Store the data buffer and address with the op. The op contains the Buffer.
                    pending_ops[slot] = Some((io_token, op));
                }
                Op::Fsync { fd } => {
                    let effective_fd = leased_fd.unwrap_or(*fd);
                    let result = match SafeFdFile::new(effective_fd) {
                        Ok(mut safe_file) => safe_file
                            .as_mut()
                            .sync_all()
                            .map(|_| CompletionKind::Fsync)
                            .map_err(IoError::Io),
                        Err(err) => Err(IoError::Io(err)),
                    };
                    self.complete_local((io_token, result));
                    descriptor_leases[slot] = None;
                    descriptor_lease_tokens[slot] = None;
                }
                Op::Close { fd } => {
                    let result = {
                        // SAFETY: Close consumes the raw fd for this operation;
                        // callers must not use it after submitting Op::Close.
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
                    // Register fd with mio for epoll-based polling
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
                        "Operation not supported by epoll backend",
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
        let udp = unsafe { &*self.udp_recv_buffers.get() }
            .iter()
            .filter(|entry| entry.is_some())
            .count();
        let completed = unsafe { &*self.completed_local.get() }.len();
        pending + udp + completed
    }

    fn drain_completions(&self) -> TaskPoll<Vec<Self::Completion>> {
        // SAFETY: We have exclusive, single-threaded access.
        let poll = unsafe { &mut *self.poll.get() };
        let events = unsafe { &mut *self.events.get() };
        let pending_ops = unsafe { &mut *self.pending_ops.get() };
        let descriptor_leases = unsafe { &mut *self.descriptor_leases.get() };
        let descriptor_lease_tokens = unsafe { &mut *self.descriptor_lease_tokens.get() };
        let completed_local = unsafe { &mut *self.completed_local.get() };
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

        // Poll for events non-blockingly to avoid delays.
        if let Err(e) = poll.poll(events, Some(Duration::from_millis(0))) {
            // An error from poll is serious, but we'll just log it for now.
            eprintln!("mio poll error: {e}");
            return if completions.is_empty() {
                TaskPoll::Ready(Vec::new())
            } else {
                TaskPoll::Ready(completions)
            };
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

                let result = match &op {
                    Op::Accept { fd } => {
                        let effective_fd = leased_fd.unwrap_or(*fd);
                        // The file descriptor is a listening socket, so we can accept a connection.
                        match syscall_accept(effective_fd) {
                            Ok((new_fd, addr)) => Ok(CompletionKind::Accept {
                                // SAFETY: syscall_accept returns a fresh owned
                                // descriptor on success.
                                fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(new_fd) },
                                addr: Some(addr),
                            }),
                            Err(e) => Err(IoError::Io(e)),
                        }
                    }
                    Op::Read { fd, len, .. } => {
                        let effective_fd = leased_fd.unwrap_or(*fd);
                        let mut buffer = Buffer::with_capacity(*len);
                        let mut requeue = false;
                        let result = loop {
                            // SAFETY: buffer has at least `len` capacity and
                            // libc::read writes at most that many bytes.
                            let ret = unsafe {
                                libc::read(
                                    effective_fd,
                                    buffer.as_mut_ptr() as *mut libc::c_void,
                                    *len,
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
                            pending_ops[slot] = Some((io_token, op.clone()));
                            continue;
                        }

                        result
                    }
                    Op::Write { fd, data, .. } => {
                        let effective_fd = leased_fd.unwrap_or(*fd);
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
                            pending_ops[slot] = Some((io_token, op.clone()));
                            continue;
                        }

                        result
                    }
                    Op::WriteStatic { fd, data } => {
                        let effective_fd = leased_fd.unwrap_or(*fd);
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
                            pending_ops[slot] = Some((io_token, op.clone()));
                            continue;
                        }

                        result
                    }
                    Op::ReadFile { fd, offset, len } => {
                        // This path should ideally not be hit as these are handled sync in submit
                        // but included for completeness if event-driven file I/O were enabled.
                        // SAFETY: File temporarily wraps fd for std APIs, then
                        // into_raw_fd prevents closing so ownership stays outside.
                        let effective_fd = leased_fd.unwrap_or(*fd);
                        let mut file = unsafe { std::fs::File::from_raw_fd(effective_fd) };
                        let result = file.seek(SeekFrom::Start(*offset)).and_then(|_| {
                            let mut buf = Buffer::new_zeroed(*len);
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
                        let effective_fd = leased_fd.unwrap_or(*fd);
                        let mut file = unsafe { std::fs::File::from_raw_fd(effective_fd) };
                        let result = file
                            .seek(SeekFrom::Start(*offset))
                            .and_then(|_| file.write(data.as_ref()))
                            .map(|bytes_written| CompletionKind::WriteFile { bytes_written });
                        let _ = file.into_raw_fd();
                        result.map_err(IoError::Io)
                    }
                    Op::UdpRecv { fd, .. } => {
                        let effective_fd = leased_fd.unwrap_or(*fd);
                        // Matched by `event.token()`
                        // SAFETY: udp_recv_buffers is shard-local and only
                        // accessed by this epoll owner thread.
                        let udp_recv_buffers = unsafe { &mut *self.udp_recv_buffers.get() };
                        if let Some(mut buffer) =
                            udp_recv_buffers.get_mut(slot).and_then(Option::take)
                        {
                            // SAFETY: UdpSocket temporarily wraps fd and is
                            // converted back with into_raw_fd before drop.
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
                                            "Failed to re-register UDP socket with epoll: {reg_err}"
                                        );
                                    }
                                    udp_recv_buffers[slot] = Some(buffer);
                                    pending_ops[slot] = Some((io_token, op.clone()));
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
                        let effective_fd = leased_fd.unwrap_or(*fd);
                        let socket = unsafe { std::net::UdpSocket::from_raw_fd(effective_fd) };
                        let result = socket.send_to(data.as_ref(), *addr).map(|bytes_written| {
                            CompletionKind::UdpSend {
                                bytes_written,
                                data: data.clone(),
                            }
                        });
                        let _ = socket.into_raw_fd();
                        result.map_err(IoError::Io)
                    }
                    Op::Connect { fd, addr: _ } => {
                        let raw_fd = leased_fd.unwrap_or(*fd);
                        // SAFETY: TcpStream temporarily wraps fd to call
                        // take_error, then into_raw_fd preserves ownership.
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
                        let effective_fd = leased_fd.unwrap_or(*fd);
                        // Buffer::clone copies logical bytes, not spare capacity. Read
                        // destinations are often empty, so clone would create zero-length
                        // iovecs and make readv report EOF without reading anything.
                        let mut buffers: smallvec::SmallVec<[Buffer; 4]> = buffers
                            .iter()
                            .map(|buffer| Buffer::new_uninitialized(buffer.capacity()))
                            .collect();
                        let iovecs: Vec<libc::iovec> = buffers
                            .iter_mut()
                            .map(|buf| libc::iovec {
                                iov_base: buf.as_mut_ptr() as *mut libc::c_void,
                                iov_len: buf.capacity(),
                            })
                            .collect();
                        let mut requeue = false;
                        let result = loop {
                            // SAFETY: iovecs point to writable capacity in the
                            // cloned buffers and remain valid for this call.
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
                                    break Err(IoError::Io(err));
                                }
                                break Err(IoError::Io(err));
                            } else if ret == 0 {
                                break Ok(CompletionKind::ReadVectored {
                                    bytes_read: 0,
                                    buffers,
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
                                for buf in buffers.iter_mut() {
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
                                    buffers,
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
                            pending_ops[slot] = Some((io_token, op.clone()));
                            continue;
                        }

                        result
                    }
                    Op::WriteVectored { fd, buffers } => {
                        let effective_fd = leased_fd.unwrap_or(*fd);
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
                                pending_ops[slot] = Some((io_token, op.clone()));
                                continue;
                            }

                            result
                        }
                    }
                    // Fsync and Close are not handled in this path currently.
                    _ => continue,
                };

                // The lease is retained across every readiness requeue and is
                // released only after this terminal result is published.
                // The lease vectors are backend-owned and remain valid for
                // this owner-thread drain pass.
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
    use super::EpollBackend;
    use crate::io::{IoProvider, Op};
    use smallvec::SmallVec;

    #[test]
    fn accepted_read_keeps_descriptor_lease_after_user_close() {
        let mut fds = [0; 2];
        // SAFETY: fds points to two writable descriptor slots.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let backend = EpollBackend::new().expect("epoll backend");
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
        let leases = unsafe { &*backend.descriptor_leases.get() };
        assert!(leases.iter().any(|lease| {
            lease
                .as_ref()
                .is_some_and(|lease| unsafe { libc::fcntl(lease.as_raw_fd(), libc::F_GETFD) } >= 0)
        }));
        let _ = token;
        // SAFETY: write end remains owned by this test and is closed once.
        unsafe { libc::close(fds[1]) };
    }

    #[test]
    fn unsupported_submit_returns_error_without_completion_token() {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let backend = EpollBackend::new().expect("epoll backend");
        let result = backend.submit(Op::UdpRecvVectored {
            fd: fds[0],
            iovecs: SmallVec::new(),
        });
        assert!(matches!(result, Err(crate::io::IoError::Io(_))));
        assert!(matches!(
            backend.drain_completions(),
            std::task::Poll::Pending
        ));
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
