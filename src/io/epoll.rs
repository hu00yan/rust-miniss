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

use crate::io::{CompletionKind, IoBackend, IoError, IoToken, Op};
use mio::{Events, Interest, Poll, Token};
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::task::{Context, Poll as TaskPoll};
use std::time::Duration;

/// An `epoll` based `IoBackend` implementation using `mio`.
pub struct EpollBackend {
    /// The `mio::Poll` instance, which manages file descriptor readiness.
    poll: UnsafeCell<Poll>,
    /// A buffer for receiving events from `mio::Poll`.
    events: UnsafeCell<Events>,
    /// A map of pending operations, keyed by the `mio::Token` used for registration.
    /// The value stores the original `IoToken` and the `Op` itself.
    pending_ops: UnsafeCell<HashMap<Token, (IoToken, Op)>>,
    /// A counter to generate unique `mio::Token` values.
    next_token: UnsafeCell<usize>,
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
        Ok(Self {
            poll: UnsafeCell::new(Poll::new()?),
            events: UnsafeCell::new(Events::with_capacity(1024)),
            pending_ops: UnsafeCell::new(HashMap::new()),
            next_token: UnsafeCell::new(0),
        })
    }
}

impl IoBackend for EpollBackend {
    type Completion = (IoToken, Op, Result<CompletionKind, IoError>);

    fn submit(&self, op: Op) -> IoToken {
        let io_token = IoToken::new();

        // SAFETY: We have exclusive, single-threaded access.
        let next_token = unsafe { &mut *self.next_token.get() };
        let poll = unsafe { &mut *self.poll.get() };
        let pending_ops = unsafe { &mut *self.pending_ops.get() };

        let mio_token = Token(*next_token);
        *next_token += 1;

        let (fd, interest) = match &op {
            Op::Accept { fd } => (*fd, Interest::READABLE),
            Op::Read { fd, .. } => (*fd, Interest::READABLE),
            Op::Write { fd, .. } => (*fd, Interest::WRITABLE),
            // Fsync and Close are handled synchronously for now, as mio doesn't directly support them.
            // This is a simplification and a more robust implementation would use a blocking thread pool.
            Op::Fsync { fd } => {
                // Perform fsync and create a completion immediately.
                // This is a placeholder for true async fsync.
                // For now, we don't return it from submit, but a real implementation would.
                // This part of the code is not fully async.
                return io_token; // Not handled async
            }
            Op::Close { fd } => {
                // Same as fsync, handled synchronously.
                return io_token; // Not handled async
            }
        };

        let mut source = mio::unix::SourceFd(&fd);
        if let Err(e) = poll.registry().register(&mut source, mio_token, interest) {
            // If registration fails, we can't proceed with this op.
            // A real implementation might queue this for later.
            eprintln!("Failed to register fd with mio: {}", e);
            // This op is effectively dropped.
        } else {
            pending_ops.insert(mio_token, (io_token, op));
        }

        io_token
    }

    fn poll_complete(&self, _cx: &mut Context<'_>) -> TaskPoll<Vec<Self::Completion>> {
        // SAFETY: We have exclusive, single-threaded access.
        let poll = unsafe { &mut *self.poll.get() };
        let events = unsafe { &mut *self.events.get() };
        let pending_ops = unsafe { &mut *self.pending_ops.get() };

        // Poll for events with a non-blocking timeout.
        if let Err(e) = poll.poll(events, Some(Duration::from_millis(0))) {
            // An error from poll is serious, but we'll just log it for now.
            eprintln!("mio poll error: {}", e);
            return TaskPoll::Ready(Vec::new());
        }

        let mut completions = Vec::new();

        for event in events.iter() {
            let mio_token = event.token();
            if let Some((io_token, op)) = pending_ops.remove(&mio_token) {
                let mut source = mio::unix::SourceFd(&op.as_raw_fd());
                let _ = poll.registry().deregister(&mut source);

                let result = match &op {
                    Op::Accept { fd } => {
                        // The file descriptor is a listening socket, so we can accept a connection.
                        match syscall_accept(*fd) {
                            Ok((new_fd, addr)) => Ok(CompletionKind::Accept { fd: new_fd, addr: Some(addr) }),
                            Err(e) => Err(IoError::Io(e)),
                        }
                    }
                    Op::Read { fd, offset, len } => {
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let res = file.seek(SeekFrom::Start(*offset))
                            .and_then(|_| {
                                let mut buf = vec![0; *len];
                                file.read(&mut buf).map(|bytes_read| {
                                    buf.truncate(bytes_read);
                                    CompletionKind::Read { bytes_read, data: buf }
                                })
                            });
                        // Prevent drop from closing the file descriptor
                        std::mem::forget(file);
                        res.map_err(IoError::Io)
                    }
                    Op::Write { fd, offset, data } => {
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let res = file.seek(SeekFrom::Start(*offset))
                            .and_then(|_| file.write(data))
                            .map(|bytes_written| CompletionKind::Write { bytes_written });
                        std::mem::forget(file);
                        res.map_err(IoError::Io)
                    }
                    // Fsync and Close are not handled in this path currently.
                    _ => continue,
                };

                completions.push((io_token, op, result));
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
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of_val(&storage) as libc::socklen_t;
    let new_fd = unsafe {
        libc::accept(
            fd,
            &mut storage as *mut _ as *mut _,
            &mut len,
        )
    };

    if new_fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let addr = unsafe {
        match storage.ss_family as libc::c_int {
            libc::AF_INET => {
                let sockaddr_in = &*(storage as *const _ as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sockaddr_in.sin_addr.s_addr));
                let port = u16::from_be(sockaddr_in.sin_port);
                std::net::SocketAddr::V4(std::net::SocketAddrV4::new(ip, port))
            }
            libc::AF_INET6 => {
                let sockaddr_in6 = &*(storage as *const _ as *const libc::sockaddr_in6);
                let ip = std::net::Ipv6Addr::from(sockaddr_in6.sin6_addr.s6_addr);
                let port = u16::from_be(sockaddr_in6.sin6_port);
                std::net::SocketAddr::V6(std::net::SocketAddrV6::new(ip, port, sockaddr_in6.sin6_flowinfo, sockaddr_in6.sin6_scope_id))
            }
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "Unsupported address family")),
        }
    };

    Ok((new_fd, addr))
}

// Helper to get RawFd from Op
impl AsRawFd for Op {
    fn as_raw_fd(&self) -> RawFd {
        match *self {
            Op::Read { fd, .. } => fd,
            Op::Write { fd, .. } => fd,
            Op::Fsync { fd, .. } => fd,
            Op::Close { fd, .. } => fd,
        }
    }
}
