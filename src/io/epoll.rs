#![cfg(any(io_backend = "epoll", all(unix, not(target_os = "linux"))))]

//! An `epoll` (or `kqueue` via `mio`) backend for the I/O subsystem.
//!
//! This implementation is designed for a thread-per-core architecture, serving as a
//! fallback for Unix-like systems where `io-uring` is not available. Each `EpollBackend`
//! instance owns a `mio::Poll` handle and is intended for single-threaded use.
//!
//! Like the `uring` backend, this module uses `UnsafeCell` and `unsafe` trait impls
//! to manage thread-local state within the `IoBackend` trait's `&self` methods.

use crate::io::{CompletionKind, IoError, IoProvider, IoToken, Op};
use crate::Buffer;
use mio::{unix::SourceFd, Events, Interest, Poll, Token};
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::task::{Context, Poll as TaskPoll};
use std::time::Duration;
use threadpool::ThreadPool;

type IoCompletion = (IoToken, Op, Result<CompletionKind, IoError>);
const COMPLETION_QUEUE_CAPACITY: usize = 4096;

/// Safe file descriptor wrapper that ensures RAII compliance
#[derive(Debug)]
struct SafeFdFile {
    file: Option<std::fs::File>,
}

impl SafeFdFile {
    /// Create a new SafeFdFile from a raw fd
    fn new(fd: RawFd) -> io::Result<Self> {
        let dup_fd = unsafe { libc::dup(fd) };
        if dup_fd < 0 {
            return Err(io::Error::last_os_error());
        }

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
    /// A map of pending operations, keyed by the `mio::Token` used for registration.
    /// The value stores the original `IoToken` and the `Op` itself.
    pending_ops: UnsafeCell<HashMap<Token, (IoToken, Op)>>,
    /// A map of UDP receive buffers, keyed by the `mio::Token` used for registration.
    udp_recv_buffers: UnsafeCell<HashMap<Token, crate::buffer::Buffer>>,
    /// A counter to generate unique `mio::Token` values.
    next_token: UnsafeCell<usize>,
    /// Thread pool for blocking file I/O operations
    thread_pool: Arc<ThreadPool>,
    /// Same-thread completed operations.
    completed_local: UnsafeCell<Vec<IoCompletion>>,
    /// Completed operations from the file I/O thread pool.
    completed_tx: SyncSender<IoCompletion>,
    completed_rx: UnsafeCell<Receiver<IoCompletion>>,
}

// SAFETY: The `EpollBackend` is designed to be thread-local. It is created within a
// thread and must not be moved or accessed from another. The `Send` and `Sync` markers
// are required to satisfy the `IoBackend` trait bounds. The thread-per-core
// architecture of the runtime ensures this is safe.
unsafe impl Send for EpollBackend {}
unsafe impl Sync for EpollBackend {}

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
            pending_ops: UnsafeCell::new(HashMap::new()),
            udp_recv_buffers: UnsafeCell::new(HashMap::new()),
            next_token: UnsafeCell::new(0),
            thread_pool,
            completed_local: UnsafeCell::new(Vec::new()),
            completed_tx,
            completed_rx: UnsafeCell::new(completed_rx),
        })
    }

    fn complete_local(&self, completion: IoCompletion) {
        let completed_local = unsafe { &mut *self.completed_local.get() };
        completed_local.push(completion);
    }
}

impl IoProvider for EpollBackend {
    type Completion = (IoToken, Op, Result<CompletionKind, IoError>);

    fn submit(&self, op: Op) -> IoToken {
        let io_token = IoToken::new();

        // SAFETY: We have exclusive, single-threaded access.
        let next_token = unsafe { &mut *self.next_token.get() };
        let poll = unsafe { &mut *self.poll.get() };
        let pending_ops = unsafe { &mut *self.pending_ops.get() };

        let mio_token = Token(*next_token);
        *next_token += 1;

        match &op {
            Op::Accept { fd } => {
                let mut source = mio::unix::SourceFd(fd);
                if let Err(e) = poll
                    .registry()
                    .register(&mut source, mio_token, Interest::READABLE)
                {
                    eprintln!("Failed to register fd with mio: {e}");
                } else {
                    pending_ops.insert(mio_token, (io_token, op));
                }
            }
            Op::Read { fd, .. } => {
                let mut source = mio::unix::SourceFd(fd);
                if let Err(e) = poll
                    .registry()
                    .register(&mut source, mio_token, Interest::READABLE)
                {
                    eprintln!("Failed to register fd with mio: {e}");
                } else {
                    pending_ops.insert(mio_token, (io_token, op));
                }
            }
            Op::Write { fd, .. } | Op::WriteStatic { fd, .. } => {
                let mut source = mio::unix::SourceFd(fd);
                if let Err(e) = poll
                    .registry()
                    .register(&mut source, mio_token, Interest::WRITABLE)
                {
                    eprintln!("Failed to register fd with mio: {e}");
                } else {
                    pending_ops.insert(mio_token, (io_token, op));
                }
            }
            Op::ReadVectored { fd, .. } => {
                let mut source = mio::unix::SourceFd(fd);
                if let Err(e) = poll
                    .registry()
                    .register(&mut source, mio_token, Interest::READABLE)
                {
                    eprintln!("Failed to register fd with mio: {e}");
                } else {
                    pending_ops.insert(mio_token, (io_token, op));
                }
            }
            Op::WriteVectored { fd, .. } => {
                let mut source = mio::unix::SourceFd(fd);
                if let Err(e) = poll
                    .registry()
                    .register(&mut source, mio_token, Interest::WRITABLE)
                {
                    eprintln!("Failed to register fd with mio: {e}");
                } else {
                    pending_ops.insert(mio_token, (io_token, op));
                }
            }
            Op::Connect { fd, addr: _ } => {
                let mut source = mio::unix::SourceFd(fd);
                if let Err(e) = poll
                    .registry()
                    .register(&mut source, mio_token, Interest::WRITABLE)
                {
                    eprintln!("Failed to register fd for connect: {e}");
                } else {
                    pending_ops.insert(mio_token, (io_token, op));
                }
            }
            // File operations are handled asynchronously in thread pool
            Op::ReadFile { fd, offset, len } => {
                let completed_tx = self.completed_tx.clone();
                let thread_pool = self.thread_pool.clone();
                let op_clone = op.clone();
                let io_token_copy = io_token;
                let fd_copy = *fd;
                let offset_copy = *offset;
                let len_copy = *len;

                thread_pool.execute(move || {
                    let result = match SafeFdFile::new(fd_copy) {
                        Ok(mut safe_file) => safe_file
                            .as_mut()
                            .seek(SeekFrom::Start(offset_copy))
                            .and_then(|_| {
                                let mut buf = Buffer::new_zeroed(len_copy);
                                safe_file.as_mut().read(buf.as_mut()).map(|bytes_read| {
                                    unsafe {
                                        buf.set_len(bytes_read);
                                    }
                                    CompletionKind::ReadFile {
                                        bytes_read,
                                        data: buf,
                                    }
                                })
                            })
                            .map_err(IoError::Io),
                        Err(err) => Err(IoError::Io(err)),
                    };

                    let _ = completed_tx.send((io_token_copy, op_clone, result));
                });
            }
            Op::WriteFile { fd, offset, data } => {
                let completed_tx = self.completed_tx.clone();
                let thread_pool = self.thread_pool.clone();
                let op_clone = op.clone();
                let io_token_copy = io_token;
                let fd_copy = *fd;
                let offset_copy = *offset;
                let data_clone = data.clone();

                thread_pool.execute(move || {
                    let result =
                        match SafeFdFile::new(fd_copy) {
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

                    let _ = completed_tx.send((io_token_copy, op_clone, result));
                });
            }
            Op::UdpRecv { fd, buffer } => {
                // Register the UDP socket for read events
                let mut source = mio::unix::SourceFd(fd);
                if let Err(e) =
                    poll.registry()
                        .register(&mut source, mio_token, mio::Interest::READABLE)
                {
                    eprintln!("Failed to register UDP socket with epoll: {e}");
                }
                // Store the buffer for use in drain_completions
                let udp_recv_buffers = unsafe { &mut *self.udp_recv_buffers.get() };
                udp_recv_buffers.insert(mio_token, buffer.clone());
                pending_ops.insert(mio_token, (io_token, op));
            }
            Op::UdpSend {
                fd,
                data: _data,
                addr: _addr,
            } => {
                // Register the UDP socket for write events, make it asynchronous
                let mut source = mio::unix::SourceFd(fd);
                if let Err(e) =
                    poll.registry()
                        .register(&mut source, mio_token, mio::Interest::WRITABLE)
                {
                    eprintln!("Failed to register UDP socket with epoll for send: {e}");
                }
                // Store the data buffer and address with the op. The op contains the Buffer.
                pending_ops.insert(mio_token, (io_token, op));
            }
            Op::Fsync { fd } => {
                let result = match SafeFdFile::new(*fd) {
                    Ok(mut safe_file) => safe_file
                        .as_mut()
                        .sync_all()
                        .map(|_| CompletionKind::Fsync)
                        .map_err(IoError::Io),
                    Err(err) => Err(IoError::Io(err)),
                };
                self.complete_local((io_token, op.clone(), result));
            }
            Op::Close { fd } => {
                let result = {
                    let res = unsafe { libc::close(*fd) };
                    if res == -1 {
                        Err(IoError::Io(io::Error::last_os_error()))
                    } else {
                        Ok(CompletionKind::Close)
                    }
                };
                self.complete_local((io_token, op.clone(), result));
            }
            Op::PollAdd {
                fd,
                events,
                multishot: _,
            } => {
                // Register fd with mio for epoll-based polling
                let mut source = mio::unix::SourceFd(fd);
                let events_u32 = *events;
                let interest = if events_u32 & libc::POLLIN as u32 != 0 {
                    Interest::READABLE
                } else if events_u32 & libc::POLLOUT as u32 != 0 {
                    Interest::WRITABLE
                } else {
                    Interest::READABLE | Interest::WRITABLE
                };
                if let Err(e) = poll.registry().register(&mut source, mio_token, interest) {
                    eprintln!("Failed to register fd with mio for poll: {e}");
                } else {
                    pending_ops.insert(mio_token, (io_token, op));
                }
            }
            _ => {
                // For unsupported operations, return an error immediately
                let result = Err(IoError::Io(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Operation not supported by epoll backend",
                )));
                self.complete_local((io_token, op, result));
            }
        };

        io_token
    }

    fn drain_completions(&self, _cx: &mut Context<'_>) -> TaskPoll<Vec<Self::Completion>> {
        // SAFETY: We have exclusive, single-threaded access.
        let poll = unsafe { &mut *self.poll.get() };
        let events = unsafe { &mut *self.events.get() };
        let pending_ops = unsafe { &mut *self.pending_ops.get() };
        let completed_local = unsafe { &mut *self.completed_local.get() };
        let completed_rx = unsafe { &mut *self.completed_rx.get() };

        let mut completions = mem::take(completed_local);

        // First, collect any completed operations from the thread pool
        loop {
            match completed_rx.try_recv() {
                Ok(completion) => completions.push(completion),
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
            if let Some((io_token, op)) = pending_ops.remove(&mio_token) {
                let raw_fd = op.as_raw_fd();
                let mut source = SourceFd(&raw_fd);
                let _ = poll.registry().deregister(&mut source);

                let result = match &op {
                    Op::Accept { fd } => {
                        // The file descriptor is a listening socket, so we can accept a connection.
                        match syscall_accept(*fd) {
                            Ok((new_fd, addr)) => Ok(CompletionKind::Accept {
                                fd: new_fd,
                                addr: Some(addr),
                            }),
                            Err(e) => Err(IoError::Io(e)),
                        }
                    }
                    Op::Read { fd, len, .. } => {
                        let mut buffer = Buffer::with_capacity(*len);
                        let mut requeue = false;
                        let result = loop {
                            let ret = unsafe {
                                libc::read(*fd, buffer.as_mut_ptr() as *mut libc::c_void, *len)
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
                                unsafe {
                                    buffer.set_len(bytes_read);
                                }
                                break Ok(CompletionKind::Read {
                                    bytes_read,
                                    data: buffer,
                                });
                            }
                        };

                        if requeue {
                            let mut source = SourceFd(fd);
                            if let Err(reg_err) =
                                poll.registry()
                                    .register(&mut source, mio_token, Interest::READABLE)
                            {
                                eprintln!(
                                    "Failed to re-register fd for read after WouldBlock: {reg_err}"
                                );
                            }
                            pending_ops.insert(mio_token, (io_token, op.clone()));
                            continue;
                        }

                        result
                    }
                    Op::Write { fd, data, .. } => {
                        let mut written = 0usize;
                        let mut requeue = false;
                        let result = loop {
                            let remaining = &data[written..];
                            if remaining.is_empty() {
                                break Ok(CompletionKind::Write {
                                    bytes_written: written,
                                });
                            }
                            let ret = unsafe {
                                libc::write(
                                    *fd,
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
                            let mut source = SourceFd(fd);
                            if let Err(reg_err) =
                                poll.registry()
                                    .register(&mut source, mio_token, Interest::WRITABLE)
                            {
                                eprintln!(
                                    "Failed to re-register fd for write after WouldBlock: {reg_err}"
                                );
                            }
                            pending_ops.insert(mio_token, (io_token, op.clone()));
                            continue;
                        }

                        result
                    }
                    Op::WriteStatic { fd, data } => {
                        let mut written = 0usize;
                        let mut requeue = false;
                        let result = loop {
                            let remaining = &data[written..];
                            if remaining.is_empty() {
                                break Ok(CompletionKind::Write {
                                    bytes_written: written,
                                });
                            }
                            let ret = unsafe {
                                libc::write(
                                    *fd,
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
                            let mut source = SourceFd(fd);
                            if let Err(reg_err) =
                                poll.registry()
                                    .register(&mut source, mio_token, Interest::WRITABLE)
                            {
                                eprintln!(
                                    "Failed to re-register fd for static write after WouldBlock: {reg_err}"
                                );
                            }
                            pending_ops.insert(mio_token, (io_token, op.clone()));
                            continue;
                        }

                        result
                    }
                    Op::ReadFile { fd, offset, len } => {
                        // This path should ideally not be hit as these are handled sync in submit
                        // but included for completeness if event-driven file I/O were enabled.
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let result = file.seek(SeekFrom::Start(*offset)).and_then(|_| {
                            let mut buf = Buffer::new_zeroed(*len);
                            file.read(buf.as_mut()).map(|bytes_read| {
                                unsafe {
                                    buf.set_len(bytes_read);
                                }
                                CompletionKind::ReadFile {
                                    bytes_read,
                                    data: buf,
                                }
                            })
                        });
                        let _ = file.into_raw_fd();
                        result.map_err(IoError::Io)
                    }
                    Op::WriteFile { fd, offset, data } => {
                        // This path should ideally not be hit.
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let result = file
                            .seek(SeekFrom::Start(*offset))
                            .and_then(|_| file.write(data.as_ref()))
                            .map(|bytes_written| CompletionKind::WriteFile { bytes_written });
                        let _ = file.into_raw_fd();
                        result.map_err(IoError::Io)
                    }
                    Op::UdpRecv { fd, .. } => {
                        // Matched by `event.token()`
                        let udp_recv_buffers = unsafe { &mut *self.udp_recv_buffers.get() };
                        if let Some(mut buffer) = udp_recv_buffers.remove(&mio_token) {
                            let socket = unsafe { std::net::UdpSocket::from_raw_fd(*fd) };
                            let mut retry = false;
                            let result = match socket.recv_from(buffer.as_mut()) {
                                Ok((bytes_read, addr)) => {
                                    unsafe {
                                        buffer.set_len(bytes_read);
                                    }
                                    Ok(CompletionKind::UdpRecv {
                                        bytes_read,
                                        buffer,
                                        addr,
                                    })
                                }
                                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                    let mut mio_source = SourceFd(fd);
                                    if let Err(reg_err) = poll.registry().register(
                                        &mut mio_source,
                                        mio_token,
                                        mio::Interest::READABLE,
                                    ) {
                                        eprintln!(
                                            "Failed to re-register UDP socket with epoll: {reg_err}"
                                        );
                                    }
                                    udp_recv_buffers.insert(mio_token, buffer);
                                    pending_ops.insert(mio_token, (io_token, op.clone()));
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

                        let socket = unsafe { std::net::UdpSocket::from_raw_fd(*fd) };
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
                        let raw_fd = *fd;
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
                        let mut buffers = buffers.clone();
                        let iovecs: Vec<libc::iovec> = buffers
                            .iter_mut()
                            .map(|buf| libc::iovec {
                                iov_base: buf.as_mut_ptr() as *mut libc::c_void,
                                iov_len: buf.capacity(),
                            })
                            .collect();
                        let mut requeue = false;
                        let result = loop {
                            let ret =
                                unsafe { libc::readv(*fd, iovecs.as_ptr(), iovecs.len() as i32) };
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
                                let mut remaining = bytes_read;
                                for buf in buffers.iter_mut() {
                                    let can_read = std::cmp::min(remaining, buf.capacity());
                                    unsafe {
                                        buf.set_len(can_read);
                                    }
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
                            let mut source = SourceFd(fd);
                            if let Err(reg_err) =
                                poll.registry()
                                    .register(&mut source, mio_token, Interest::READABLE)
                            {
                                eprintln!(
                                    "Failed to re-register fd for readv after WouldBlock: {reg_err}"
                                );
                            }
                            pending_ops.insert(mio_token, (io_token, op.clone()));
                            continue;
                        }

                        result
                    }
                    Op::WriteVectored { fd, buffers } => {
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
                                let ret = unsafe {
                                    libc::writev(*fd, iovecs.as_ptr(), iovecs.len() as i32)
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
                                let mut source = SourceFd(fd);
                                if let Err(reg_err) = poll.registry().register(
                                    &mut source,
                                    mio_token,
                                    Interest::WRITABLE,
                                ) {
                                    eprintln!(
                                        "Failed to re-register fd for writev after WouldBlock: {reg_err}"
                                    );
                                }
                                pending_ops.insert(mio_token, (io_token, op.clone()));
                                continue;
                            }

                            result
                        }
                    }
                    // Fsync and Close are not handled in this path currently.
                    _ => continue,
                };

                completions.push((io_token, op, result));
            }
        }

        // Handle any pending synchronous operations (Fsync, Close) - ReadFile/WriteFile now handled by thread pool
        // We need to create a temporary vector to avoid borrowing issues
        let pending_tokens: Vec<_> = pending_ops.keys().cloned().collect();
        for mio_token in pending_tokens {
            if let Some((_io_token, op)) = pending_ops.get(&mio_token) {
                match op {
                    Op::Fsync { fd } => {
                        // Perform fsync synchronously
                        let result = {
                            let file = unsafe { std::fs::File::from_raw_fd(*fd) };
                            let res = file.sync_all();
                            let _ = file.into_raw_fd();
                            res.map(|_| CompletionKind::Fsync).map_err(IoError::Io)
                        };
                        // Remove from pending ops and add to completions
                        if let Some((io_token, op)) = pending_ops.remove(&mio_token) {
                            completions.push((io_token, op, result));
                        }
                    }
                    Op::Close { fd } => {
                        // Perform close synchronously
                        let result = {
                            let res = unsafe { libc::close(*fd) };
                            if res == -1 {
                                Err(IoError::Io(io::Error::last_os_error()))
                            } else {
                                Ok(CompletionKind::Close)
                            }
                        };
                        // Remove from pending ops and add to completions
                        if let Some((io_token, op)) = pending_ops.remove(&mio_token) {
                            completions.push((io_token, op, result));
                        }
                    }
                    _ => {} // Other operations are handled by mio events or thread pool
                }
            }
        }

        if completions.is_empty() {
            TaskPoll::Pending
        } else {
            TaskPoll::Ready(completions)
        }
    }
}

fn syscall_accept(fd: RawFd) -> io::Result<(RawFd, std::net::SocketAddr)> {
    let mut storage: libc::sockaddr_storage =
        unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
    let mut len = std::mem::size_of_val(&storage) as libc::socklen_t;
    let new_fd = unsafe { libc::accept(fd, &mut storage as *mut _ as *mut _, &mut len) };

    if new_fd < 0 {
        return Err(io::Error::last_os_error());
    }

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
                ))
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
