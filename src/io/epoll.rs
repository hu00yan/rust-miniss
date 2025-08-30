#![cfg(any(
    all(unix, not(target_os = "linux")),
    all(target_os = "linux", io_backend = "epoll")
))]

//! An `epoll` (or `kqueue` via `mio`) backend for the I/O subsystem.
//!
//! This implementation is designed for a thread-per-core architecture, serving as a
//! fallback for Unix-like systems where `io-uring` is not available. Each `EpollBackend`
//! instance owns a `mio::Poll` handle and is intended for single-threaded use.
//!
//! Like the `uring` backend, this module uses `UnsafeCell` and `unsafe` trait impls
//! to manage thread-local state within the `IoBackend` trait's `&self` methods.

use crate::io::{CompletionKind, IoError, IoProvider, IoToken, Op};
use mio::{unix::SourceFd, Events, Interest, Poll, Token};
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::sync::Arc;
use std::task::{Context, Poll as TaskPoll};
use std::time::Duration;
use threadpool::ThreadPool;

/// Safe file descriptor wrapper that ensures RAII compliance
#[derive(Debug)]
struct SafeFdFile {
    file: Option<std::fs::File>,
    fd: RawFd,
}

impl SafeFdFile {
    /// Create a new SafeFdFile from a raw fd
    fn new(fd: RawFd) -> Self {
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        Self {
            file: Some(file),
            fd,
        }
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
    /// Completed operations from the thread pool
    completed_ops: UnsafeCell<Vec<(IoToken, Op, Result<CompletionKind, IoError>)>>,
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

        Ok(Self {
            poll: UnsafeCell::new(Poll::new()?),
            events: UnsafeCell::new(Events::with_capacity(1024)),
            pending_ops: UnsafeCell::new(HashMap::new()),
            udp_recv_buffers: UnsafeCell::new(HashMap::new()),
            next_token: UnsafeCell::new(0),
            thread_pool,
            completed_ops: UnsafeCell::new(Vec::new()),
        })
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
                    eprintln!("Failed to register fd with mio: {}", e);
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
                    eprintln!("Failed to register fd with mio: {}", e);
                } else {
                    pending_ops.insert(mio_token, (io_token, op));
                }
            }
            Op::Write { fd, .. } => {
                let mut source = mio::unix::SourceFd(fd);
                if let Err(e) = poll
                    .registry()
                    .register(&mut source, mio_token, Interest::WRITABLE)
                {
                    eprintln!("Failed to register fd with mio: {}", e);
                } else {
                    pending_ops.insert(mio_token, (io_token, op));
                }
            }
            // File operations are handled asynchronously in thread pool
            Op::ReadFile { fd, offset, len } => {
                let completed_ops_ptr = self.completed_ops.get();
                let thread_pool = self.thread_pool.clone();
                let op_clone = op.clone();
                let io_token_copy = io_token;
                let fd_copy = *fd;
                let offset_copy = *offset;
                let len_copy = *len;

                thread_pool.execute(move || {
                    let result = {
                        // Use SafeFdFile to ensure RAII compliance
                        let mut safe_file = SafeFdFile::new(fd_copy);
                        let res = safe_file
                            .as_mut()
                            .seek(SeekFrom::Start(offset_copy))
                            .and_then(|_| {
                                let mut buf = crate::buffer::BufferPool::get(len_copy);
                                safe_file.as_mut().write(&buf).map(|bytes_written| {
                                    CompletionKind::WriteFile { bytes_written }
                                })
                            });
                        // SafeFdFile will be automatically dropped here, ensuring fd is closed
                        res.map_err(IoError::Io)
                    };

                    // SAFETY: We know this pointer is valid as long as EpollBackend exists
                    unsafe {
                        (*completed_ops_ptr).push((io_token_copy, op_clone, result));
                    }
                });
            }
            Op::WriteFile { fd, offset, data } => {
                let completed_ops_ptr = self.completed_ops.get();
                let thread_pool = self.thread_pool.clone();
                let op_clone = op.clone();
                let io_token_copy = io_token;
                let fd_copy = *fd;
                let offset_copy = *offset;
                let data_clone = data.clone();

                thread_pool.execute(move || {
                    let result = {
                        use std::os::unix::io::FromRawFd;
                        let mut safe_file = SafeFdFile::new(fd_copy);
                        let res = safe_file
                            .as_mut()
                            .seek(SeekFrom::Start(offset_copy))
                            .and_then(|_| {
                                let mut buf = crate::buffer::BufferPool::get(len_copy);
                                safe_file.as_mut().write(&buf).map(|bytes_written| {
                                    CompletionKind::WriteFile { bytes_written }
                                })
                            });
                        // SafeFdFile will be automatically dropped here
                        res.map_err(IoError::Io)
                    };

                    // SAFETY: We know this pointer is valid as long as EpollBackend exists
                    unsafe {
                        (*completed_ops_ptr).push((io_token_copy, op_clone, result));
                    }
                });
            }
            Op::UdpRecv { fd, buffer } => {
                // Register the UDP socket for read events
                let mut source = mio::unix::SourceFd(fd);
                if let Err(e) =
                    poll.registry()
                        .register(&mut source, mio_token, mio::Interest::READABLE)
                {
                    eprintln!("Failed to register UDP socket with epoll: {}", e);
                }
                // Store the buffer for use in poll_complete
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
                    eprintln!("Failed to register UDP socket with epoll for send: {}", e);
                }
                // Store the data buffer and address with the op. The op contains the Buffer.
                pending_ops.insert(mio_token, (io_token, op));
            }
            Op::Fsync { fd } => {
                // Fsync remains synchronous.
                let _result = {
                    use std::os::unix::io::FromRawFd;
                    let safe_file = SafeFdFile::new(*fd);
                    let res = safe_file.as_mut().metadata().map(|_| CompletionKind::Fsync);
                    // SafeFdFile will be automatically dropped here
                    res.map(|_| CompletionKind::Fsync).map_err(IoError::Io)
                };
                pending_ops.insert(mio_token, (io_token, op));
            }
            Op::Close { fd } => {
                // Close remains synchronous.
                let _result = {
                    let res = unsafe { libc::close(*fd) };
                    if res == -1 {
                        Err(IoError::Io(io::Error::last_os_error()))
                    } else {
                        Ok(CompletionKind::Close)
                    }
                };
                pending_ops.insert(mio_token, (io_token, op));
            }
        };

        io_token
    }

    fn poll_complete(&self, _cx: &mut Context<'_>) -> TaskPoll<Vec<Self::Completion>> {
        // SAFETY: We have exclusive, single-threaded access.
        let poll = unsafe { &mut *self.poll.get() };
        let events = unsafe { &mut *self.events.get() };
        let pending_ops = unsafe { &mut *self.pending_ops.get() };
        let completed_ops = unsafe { &mut *self.completed_ops.get() };

        let mut completions = Vec::new();

        // First, collect any completed operations from the thread pool
        completions.extend(completed_ops.drain(..));

        // Poll for events with a non-blocking timeout.
        if let Err(e) = poll.poll(events, Some(Duration::from_millis(0))) {
            // An error from poll is serious, but we'll just log it for now.
            eprintln!("mio poll error: {}", e);
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
                let mut source = SourceFd(&op.as_raw_fd());
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
                    Op::Read { fd, offset, len } => {
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let res = file.seek(SeekFrom::Start(*offset)).and_then(|_| {
                            let mut buf = crate::buffer::BufferPool::get(*len);
                            file.read(buf.as_mut()).map(|bytes_read| {
                                unsafe {
                                    buf.set_len(bytes_read);
                                }
                                CompletionKind::Read {
                                    bytes_read,
                                    data: buf,
                                }
                            })
                        });
                        // Explicitly close the file to avoid fd conflicts
                        drop(file);
                        res.map_err(IoError::Io)
                    }
                    Op::Write { fd, offset, data } => {
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let res = file
                            .seek(SeekFrom::Start(*offset))
                            .and_then(|_| file.write(data.as_ref())) // Write from Buffer
                            .map(|bytes_written| CompletionKind::Write { bytes_written });
                        // Explicitly close the file to avoid fd conflicts
                        drop(file);
                        res.map_err(IoError::Io)
                    }
                    Op::ReadFile { fd, offset, len } => {
                        // This path should ideally not be hit as these are handled sync in submit
                        // but included for completeness if event-driven file I/O were enabled.
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let res = file.seek(SeekFrom::Start(*offset)).and_then(|_| {
                            let mut buf = crate::buffer::BufferPool::get(*len);
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
                        // Explicitly close the file to avoid fd conflicts
                        drop(file);
                        res.map_err(IoError::Io)
                    }
                    Op::WriteFile { fd, offset, data } => {
                        // This path should ideally not be hit.
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let res = file
                            .seek(SeekFrom::Start(*offset))
                            .and_then(|_| file.write(data.as_ref()))
                            .map(|bytes_written| CompletionKind::WriteFile { bytes_written });
                        // Explicitly drop the file to ensure fd is properly closed
                        drop(file);
                        res.map_err(IoError::Io)
                    }
                    Op::UdpRecv { fd, .. } => {
                        // Matched by `event.token()`
                        let udp_recv_buffers = unsafe { &mut *self.udp_recv_buffers.get() };
                        if let Some(mut buffer) = udp_recv_buffers.remove(&mio_token) {
                            use std::os::unix::io::FromRawFd;
                            let socket = unsafe { std::net::UdpSocket::from_raw_fd(*fd) };
                            let res = match socket.recv_from(buffer.as_mut()) {
                                Ok((bytes_read, addr)) => {
                                    unsafe {
                                        buffer.set_len(bytes_read);
                                    } // Safety: bytes_read <= buffer.capacity()
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
                                            "Failed to re-register UDP socket with epoll: {}",
                                            reg_err
                                        );
                                    }
                                    udp_recv_buffers.insert(mio_token, buffer); // Re-insert the buffer
                                    pending_ops.insert(mio_token, (io_token, op.clone())); // Re-insert the original op
                                    continue; // Don't add to completions if WouldBlock
                                }
                                Err(e) => Err(e),
                            }
                            .map_err(IoError::Io);
                            // Socket will be automatically dropped here
                            res
                        } else {
                            Err(IoError::Other(
                                "UDP receive buffer not found for event".to_string(),
                            ))
                        }
                    }
                    Op::UdpSend { fd, data, addr } => {
                        // Perform UDP send operation
                        use std::os::unix::io::FromRawFd;
                        let socket = unsafe { std::net::UdpSocket::from_raw_fd(*fd) };
                        let res = socket.send_to(data.as_ref(), *addr).map(|bytes_written| {
                            CompletionKind::UdpSend {
                                bytes_written,
                                data: data.clone(),
                            }
                        });
                        // Explicitly drop the socket to ensure fd is properly closed
                        drop(socket);
                        res.map_err(IoError::Io)
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
                            use std::os::unix::io::FromRawFd;
                            let file = unsafe { std::fs::File::from_raw_fd(*fd) };
                            let res = file.sync_all();
                            // Explicitly close the file to avoid fd conflicts
                            drop(file); // Prevent drop from closing the fd
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
    let mut storage: libc::sockaddr_storage = std::mem::MaybeUninit::zeroed().assume_init();
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
            Op::Fsync { fd, .. } => fd,
            Op::Close { fd, .. } => fd,
            Op::ReadFile { fd, .. } => fd,
            Op::WriteFile { fd, .. } => fd,
            Op::UdpRecv { fd, .. } => fd, // Add UdpRecv fd
            Op::UdpSend { fd, .. } => fd, // Add UdpSend fd
        }
    }
}
