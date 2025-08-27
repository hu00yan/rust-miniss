#![cfg(all(target_os = "macos", io_backend = "kqueue"))]

//! A `kqueue` backend for the I/O subsystem on macOS.
//!
//! This implementation is designed for a thread-per-core architecture, serving as a
//! fallback for macOS. Each `KqueueBackend` instance owns a `mio::Poll` handle and is
//! intended for single-threaded use. It is functionally identical to the epoll backend,
//! as `mio` provides a common abstraction over both.

use crate::io::{CompletionKind, IoBackend, IoError, IoToken, Op};
use mio::{Events, Interest, Poll, Token};
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::task::{Context, Poll as TaskPoll};
use std::time::Duration;

/// A `kqueue` based `IoBackend` implementation using `mio`.
pub struct KqueueBackend {
    poll: UnsafeCell<Poll>,
    events: UnsafeCell<Events>,
    pending_ops: UnsafeCell<HashMap<Token, (IoToken, Op)>>,
    next_token: UnsafeCell<usize>,
}

// SAFETY: The `KqueueBackend` is designed to be thread-local.
unsafe impl Send for KqueueBackend {}
unsafe impl Sync for KqueueBackend {}

impl KqueueBackend {
    /// Creates a new `KqueueBackend`.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            poll: UnsafeCell::new(Poll::new()?),
            events: UnsafeCell::new(Events::with_capacity(1024)),
            pending_ops: UnsafeCell::new(HashMap::new()),
            next_token: UnsafeCell::new(0),
        })
    }
}

impl IoBackend for KqueueBackend {
    type Completion = (IoToken, Op, Result<CompletionKind, IoError>);

    fn submit(&self, op: Op) -> IoToken {
        let io_token = IoToken::new();
        let next_token = unsafe { &mut *self.next_token.get() };
        let poll = unsafe { &mut *self.poll.get() };
        let pending_ops = unsafe { &mut *self.pending_ops.get() };
        let mio_token = Token(*next_token);
        *next_token += 1;

        match &op {
            Op::Accept { fd } => {
                let mut source = mio::unix::SourceFd(fd);
                if let Err(e) = poll.registry().register(&mut source, mio_token, Interest::READABLE) {
                    eprintln!("Failed to register fd with mio (kqueue): {}", e);
                } else {
                    pending_ops.insert(mio_token, (io_token, op));
                }
            }
            Op::Read { fd, .. } => {
                let mut source = mio::unix::SourceFd(fd);
                if let Err(e) = poll.registry().register(&mut source, mio_token, Interest::READABLE) {
                    eprintln!("Failed to register fd with mio (kqueue): {}", e);
                } else {
                    pending_ops.insert(mio_token, (io_token, op));
                }
            }
            Op::Write { fd, .. } => {
                let mut source = mio::unix::SourceFd(fd);
                if let Err(e) = poll.registry().register(&mut source, mio_token, Interest::WRITABLE) {
                    eprintln!("Failed to register fd with mio (kqueue): {}", e);
                } else {
                    pending_ops.insert(mio_token, (io_token, op));
                }
            }
            // File operations are handled synchronously
            Op::ReadFile { fd, offset, len } => {
                // Perform readFile synchronously and create a completion immediately.
                let result = {
                    use std::os::unix::io::FromRawFd;
                    let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                    let res = file.seek(SeekFrom::Start(*offset))
                        .and_then(|_| {
                            let mut buf = vec![0; *len];
                            file.read(&mut buf).map(|bytes_read| {
                                buf.truncate(bytes_read);
                                CompletionKind::ReadFile { bytes_read, data: buf }
                            })
                        });
                    std::mem::forget(file); // Prevent drop from closing the fd
                    res.map_err(IoError::Io)
                };
                // We still need to track this as a pending operation so poll_complete can return it
                pending_ops.insert(mio_token, (io_token, op));
                // We'll handle this synchronously in poll_complete
            }
            Op::WriteFile { fd, offset, data } => {
                // Perform writeFile synchronously and create a completion immediately.
                let result = {
                    use std::os::unix::io::FromRawFd;
                    let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                    let res = file.seek(SeekFrom::Start(*offset))
                        .and_then(|_| file.write(data))
                        .map(|bytes_written| CompletionKind::WriteFile { bytes_written });
                    std::mem::forget(file); // Prevent drop from closing the fd
                    res.map_err(IoError::Io)
                };
                // We still need to track this as a pending operation so poll_complete can return it
                pending_ops.insert(mio_token, (io_token, op));
                // We'll handle this synchronously in poll_complete
            }
            Op::Fsync { fd } => {
                // Perform fsync synchronously and create a completion immediately.
                let result = {
                    use std::os::unix::io::FromRawFd;
                    let file = unsafe { std::fs::File::from_raw_fd(*fd) };
                    let res = file.sync_all();
                    std::mem::forget(file); // Prevent drop from closing the fd
                    res.map(|_| CompletionKind::Fsync).map_err(IoError::Io)
                };
                // We still need to track this as a pending operation so poll_complete can return it
                pending_ops.insert(mio_token, (io_token, op));
                // We'll handle this synchronously in poll_complete
            }
            Op::Close { fd } => {
                // Perform close synchronously and create a completion immediately.
                let result = {
                    let res = unsafe { libc::close(*fd) };
                    if res == -1 {
                        Err(IoError::Io(io::Error::last_os_error()))
                    } else {
                        Ok(CompletionKind::Close)
                    }
                };
                // We still need to track this as a pending operation so poll_complete can return it
                pending_ops.insert(mio_token, (io_token, op));
                // We'll handle this synchronously in poll_complete
            }
        }

        io_token
    }

    fn poll_complete(&self, _cx: &mut Context<'_>) -> TaskPoll<Vec<Self::Completion>> {
        let poll = unsafe { &mut *self.poll.get() };
        let events = unsafe { &mut *self.events.get() };
        let pending_ops = unsafe { &mut *self.pending_ops.get() };

        if let Err(e) = poll.poll(events, Some(Duration::from_millis(0))) {
            eprintln!("mio poll error (kqueue): {}", e);
            return TaskPoll::Ready(Vec::new());
        }

        let mut completions = Vec::new();

        // First, handle any events from mio
        for event in events.iter() {
            let mio_token = event.token();
            if let Some((io_token, op)) = pending_ops.remove(&mio_token) {
                let mut source = mio::unix::SourceFd(&op.as_raw_fd());
                let _ = poll.registry().deregister(&mut source);

                let result = match &op {
                    Op::Accept { fd } => match syscall_accept(*fd) {
                        Ok((new_fd, addr)) => Ok(CompletionKind::Accept { fd: new_fd, addr: Some(addr) }),
                        Err(e) => Err(IoError::Io(e)),
                    },
                    Op::Read { fd, offset, len } => {
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let res = file.seek(SeekFrom::Start(*offset)).and_then(|_| {
                            let mut buf = vec![0; *len];
                            file.read(&mut buf).map(|bytes_read| {
                                buf.truncate(bytes_read);
                                CompletionKind::Read { bytes_read, data: buf }
                            })
                        });
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
                    Op::ReadFile { fd, offset, len } => {
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let res = file.seek(SeekFrom::Start(*offset)).and_then(|_| {
                            let mut buf = vec![0; *len];
                            file.read(&mut buf).map(|bytes_read| {
                                buf.truncate(bytes_read);
                                CompletionKind::ReadFile { bytes_read, data: buf }
                            })
                        });
                        std::mem::forget(file);
                        res.map_err(IoError::Io)
                    }
                    Op::WriteFile { fd, offset, data } => {
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let res = file.seek(SeekFrom::Start(*offset))
                            .and_then(|_| file.write(data))
                            .map(|bytes_written| CompletionKind::WriteFile { bytes_written });
                        std::mem::forget(file);
                        res.map_err(IoError::Io)
                    }
                    _ => continue,
                };
                completions.push((io_token, op, result));
            }
        }

        // Handle any pending synchronous operations (ReadFile, WriteFile, Fsync, Close)
        // We need to create a temporary vector to avoid borrowing issues
        let pending_tokens: Vec<_> = pending_ops.keys().cloned().collect();
        for mio_token in pending_tokens {
            if let Some((io_token, op)) = pending_ops.get(&mio_token) {
                match op {
                    Op::ReadFile { fd, offset, len } => {
                        // Perform readFile synchronously
                        let result = {
                            use std::os::unix::io::FromRawFd;
                            let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                            let res = file.seek(SeekFrom::Start(*offset))
                                .and_then(|_| {
                                    let mut buf = vec![0; *len];
                                    file.read(&mut buf).map(|bytes_read| {
                                        buf.truncate(bytes_read);
                                        CompletionKind::ReadFile { bytes_read, data: buf }
                                    })
                                });
                            std::mem::forget(file); // Prevent drop from closing the fd
                            res.map_err(IoError::Io)
                        };
                        // Remove from pending ops and add to completions
                        if let Some((io_token, op)) = pending_ops.remove(&mio_token) {
                            completions.push((io_token, op, result));
                        }
                    }
                    Op::WriteFile { fd, offset, data } => {
                        // Perform writeFile synchronously
                        let result = {
                            use std::os::unix::io::FromRawFd;
                            let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                            let res = file.seek(SeekFrom::Start(*offset))
                                .and_then(|_| file.write(data))
                                .map(|bytes_written| CompletionKind::WriteFile { bytes_written });
                            std::mem::forget(file); // Prevent drop from closing the fd
                            res.map_err(IoError::Io)
                        };
                        // Remove from pending ops and add to completions
                        if let Some((io_token, op)) = pending_ops.remove(&mio_token) {
                            completions.push((io_token, op, result));
                        }
                    }
                    Op::Fsync { fd } => {
                        // Perform fsync synchronously
                        let result = {
                            use std::os::unix::io::FromRawFd;
                            let file = unsafe { std::fs::File::from_raw_fd(*fd) };
                            let res = file.sync_all();
                            std::mem::forget(file); // Prevent drop from closing the fd
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
                    _ => {} // Other operations are handled by mio events
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
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of_val(&storage) as libc::socklen_t;
    let new_fd = unsafe { libc::accept(fd, &mut storage as *mut _ as *mut _, &mut len) };

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
        }
    }
}
