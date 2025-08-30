#![cfg(all(target_os = "macos", io_backend = "kqueue"))]

//! A `kqueue` backend for the I/O subsystem on macOS.
//!
//! This implementation is designed for a thread-per-core architecture, serving as a
//! fallback for macOS. Each `KqueueBackend` instance owns a `mio::Poll` handle and is
//! intended for single-threaded use. It is functionally identical to the epoll backend,
//! as `mio` provides a common abstraction over both.

use crate::io::{CompletionKind, IoError, IoProvider, IoToken, Op};
use mio::{unix::SourceFd, Events, Interest, Poll, Token};
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::UdpSocket as MioUdpSocket;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::task::{Context, Poll as TaskPoll};
use std::time::Duration;

/// A `kqueue` based `IoBackend` implementation using `mio`.
pub struct KqueueBackend {
    poll: UnsafeCell<Poll>,
    events: UnsafeCell<Events>,
    pending_ops: UnsafeCell<HashMap<Token, (IoToken, Op)>>,
    udp_recv_buffers: UnsafeCell<HashMap<Token, crate::buffer::Buffer>>,
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
            udp_recv_buffers: UnsafeCell::new(HashMap::new()),
            next_token: UnsafeCell::new(0),
        })
    }
}

impl IoProvider for KqueueBackend {
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
                if let Err(e) = poll
                    .registry()
                    .register(&mut source, mio_token, Interest::READABLE)
                {
                    eprintln!("Failed to register fd with mio (kqueue): {}", e);
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
                    eprintln!("Failed to register fd with mio (kqueue): {}", e);
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
                    eprintln!("Failed to register fd with mio (kqueue): {}", e);
                } else {
                    pending_ops.insert(mio_token, (io_token, op));
                }
            }
            // File operations are handled synchronously
            Op::ReadFile { fd, offset, len } => {
                // ReadFile remains synchronous but uses Buffer
                let result = {
                    use std::os::unix::io::FromRawFd;
                    let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                    let res = file.seek(SeekFrom::Start(*offset)).and_then(|_| {
                        let mut buf = crate::buffer::BufferPool::get(len); // Get a buffer
                        file.read(&mut buf).map(|bytes_read| {
                            unsafe {
                                buf.set_len(bytes_read);
                            } // Safety: bytes_read <= buf.capacity()
                            CompletionKind::ReadFile {
                                bytes_read,
                                data: buf,
                            }
                        })
                    });
                    // Explicitly drop the file to ensure fd is properly closed
                    drop(file);
                    res.map_err(IoError::Io)
                };
                pending_ops.insert(mio_token, (io_token, op)); // Still track for poll_complete pickup
            }
            Op::WriteFile { fd, offset, data } => {
                // WriteFile remains synchronous but uses Buffer
                let result = {
                    use std::os::unix::io::FromRawFd;
                    let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                    let res = file
                        .seek(SeekFrom::Start(*offset))
                        .and_then(|_| file.write(&data)) // Write from Buffer
                        .map(|bytes_written| CompletionKind::WriteFile { bytes_written });
                    // Explicitly drop the file to ensure fd is properly closed
                    drop(file);
                    data.recycle(); // Recycle the buffer
                    res.map_err(IoError::Io)
                };
                pending_ops.insert(mio_token, (io_token, op)); // Still track for poll_complete pickup
            }
            Op::UdpRecv { fd, buffer } => {
                // Register the UDP socket for read events
                let mut mio_socket = unsafe { MioUdpSocket::from_raw_fd(fd) };
                if let Err(e) =
                    poll.registry()
                        .register(&mut mio_socket, mio_token, mio::Interest::READABLE)
                {
                    eprintln!("Failed to register UDP socket with kqueue: {}", e);
                }
                // MioSocket will be automatically dropped here

                // Store the buffer for use in poll_complete
                udp_recv_buffers.insert(mio_token, buffer);
                pending_ops.insert(mio_token, (io_token, op));
            }
            Op::UdpSend { fd, data, addr } => {
                // Register the UDP socket for write events, make it asynchronous
                let mut mio_socket = unsafe { MioUdpSocket::from_raw_fd(fd) };
                if let Err(e) =
                    poll.registry()
                        .register(&mut mio_socket, mio_token, mio::Interest::WRITABLE)
                {
                    eprintln!("Failed to register UDP socket with kqueue for send: {}", e);
                }
                // MioSocket will be automatically dropped here

                // Store the data buffer and address with the op. The op contains the Buffer.
                pending_ops.insert(mio_token, (io_token, op));
            }
            Op::Fsync { fd } => {
                // Fsync remains synchronous.
                let result = {
                    use std::os::unix::io::FromRawFd;
                    let file = unsafe { std::fs::File::from_raw_fd(*fd) };
                    let res = file.sync_all();
                    // Explicitly drop the file to ensure fd is properly closed
                    drop(file);
                    res.map(|_| CompletionKind::Fsync).map_err(IoError::Io)
                };
                pending_ops.insert(mio_token, (io_token, op));
            }
            Op::Close { fd } => {
                // Close remains synchronous.
                let result = {
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
                        Ok((new_fd, addr)) => Ok(CompletionKind::Accept {
                            fd: new_fd,
                            addr: Some(addr),
                        }),
                        Err(e) => Err(IoError::Io(e)),
                    },
                    Op::Read { fd, offset, len } => {
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let res = file.seek(SeekFrom::Start(*offset)).and_then(|_| {
                            let mut buf = vec![0; *len];
                            file.read(&mut buf).map(|bytes_read| {
                                buf.truncate(bytes_read);
                                CompletionKind::Read {
                                    bytes_read,
                                    data: buf,
                                }
                            })
                        });
                        // File will be automatically dropped here
                        res.map_err(IoError::Io)
                    }
                    Op::Write { fd, offset, data } => {
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let res = file
                            .seek(SeekFrom::Start(*offset))
                            .and_then(|_| file.write(data))
                            .map(|bytes_written| CompletionKind::Write { bytes_written });
                        // File will be automatically dropped here
                        res.map_err(IoError::Io)
                    }
                    Op::Read { fd, offset, len } => {
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let res = file.seek(SeekFrom::Start(*offset)).and_then(|_| {
                            let mut buf = crate::buffer::BufferPool::get(*len);
                            file.read(&mut buf).map(|bytes_read| {
                                unsafe {
                                    buf.set_len(bytes_read);
                                }
                                CompletionKind::Read {
                                    bytes_read,
                                    data: buf,
                                }
                            })
                        });
                        // File will be automatically dropped here
                        res.map_err(IoError::Io)
                    }
                    Op::Write { fd, offset, data } => {
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let res = file
                            .seek(SeekFrom::Start(*offset))
                            .and_then(|_| file.write(&data))
                            .map(|bytes_written| CompletionKind::Write { bytes_written });
                        // File will be automatically dropped here
                        data.recycle();
                        res.map_err(IoError::Io)
                    }
                    Op::ReadFile { fd, offset, len } => {
                        // This path should ideally not be hit as these are handled sync in submit
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let res = file.seek(SeekFrom::Start(*offset)).and_then(|_| {
                            let mut buf = crate::buffer::BufferPool::get(*len);
                            file.read(&mut buf).map(|bytes_read| {
                                unsafe {
                                    buf.set_len(bytes_read);
                                }
                                CompletionKind::ReadFile {
                                    bytes_read,
                                    data: buf,
                                }
                            })
                        });
                        // File will be automatically dropped here
                        res.map_err(IoError::Io)
                    }
                    Op::WriteFile { fd, offset, data } => {
                        // This path should ideally not be hit.
                        let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                        let res = file
                            .seek(SeekFrom::Start(*offset))
                            .and_then(|_| file.write(&data))
                            .map(|bytes_written| CompletionKind::WriteFile { bytes_written });
                        // File will be automatically dropped here
                        data.recycle();
                        res.map_err(IoError::Io)
                    }
                    Op::UdpRecv { fd, .. } => {
                        // Matched by `event.token()`
                        let udp_recv_buffers = unsafe { &mut *self.udp_recv_buffers.get() };
                        if let Some(mut buffer) = udp_recv_buffers.remove(&mio_token) {
                            use std::os::unix::io::FromRawFd;
                            let socket = unsafe { std::net::UdpSocket::from_raw_fd(*fd) };
                            let res = socket
                                .recv_from(&mut buffer)
                                .map(|(bytes_read, addr)| {
                                    unsafe {
                                        buffer.set_len(bytes_read);
                                    } // Safety: bytes_read <= buffer.capacity()
                                    CompletionKind::UdpRecv {
                                        bytes_read,
                                        buffer,
                                        addr,
                                    }
                                })
                                .or_else(|e| {
                                    if e.kind() == io::ErrorKind::WouldBlock {
                                        let mut mio_socket =
                                            unsafe { MioUdpSocket::from_raw_fd(*fd) };
                                        if let Err(reg_err) = poll.registry().register(
                                            &mut mio_socket,
                                            mio_token,
                                            mio::Interest::READABLE,
                                        ) {
                                            eprintln!(
                                                "Failed to re-register UDP socket with kqueue: {}",
                                                reg_err
                                            );
                                        }
                                        std::mem::forget(mio_socket);
                                        udp_recv_buffers.insert(mio_token, buffer); // Re-insert the *original* buffer
                                        pending_ops.insert(mio_token, (io_token, op.clone())); // Re-insert the original op
                                        Err(io::Error::new(
                                            io::ErrorKind::WouldBlock,
                                            "Would block",
                                        ))
                                    } else {
                                        Err(e)
                                    }
                                });
                            // Socket will be automatically dropped here

                            if res.is_err()
                                && res.as_ref().unwrap_err().kind() == io::ErrorKind::WouldBlock
                            {
                                continue; // Don't add to completions if WouldBlock
                            }
                            res.map_err(IoError::Io)
                        } else {
                            Err(IoError::Other(
                                "UDP receive buffer not found for event".to_string(),
                            ))
                        }
                    }
                    Op::UdpSend { fd, data, addr } => {
                        // Matched by `event.token()`
                        use std::os::unix::io::FromRawFd;
                        let socket = unsafe { std::net::UdpSocket::from_raw_fd(*fd) };
                        let res = socket
                            .send_to(&data, *addr) // Use &data as data is Buffer
                            .map(|bytes_written| CompletionKind::UdpSend {
                                bytes_written,
                                data: data,
                            }); // Move data for recycling
                        std::mem::forget(socket);
                        res.map_err(IoError::Io)
                    }
                    _ => continue,
                };
                completions.push((io_token, op, result));
            }
        }

        // Handle any pending synchronous operations (ReadFile, WriteFile, Fsync, Close)
        let mut sync_completions_to_add = Vec::new();
        let pending_ops_snapshot: Vec<_> = pending_ops.drain().collect(); // Drain and re-insert if not completed
        for (mio_token, (io_token, op)) in pending_ops_snapshot {
            match op {
                Op::ReadFile { fd, offset, len } => {
                    let result = {
                        use std::os::unix::io::FromRawFd;
                        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
                        let res = file.seek(SeekFrom::Start(offset)).and_then(|_| {
                            let mut buf = crate::buffer::BufferPool::get(len);
                            file.read(&mut buf).map(|bytes_read| {
                                unsafe {
                                    buf.set_len(bytes_read);
                                }
                                CompletionKind::ReadFile {
                                    bytes_read,
                                    data: buf,
                                }
                            })
                        });
                        // File will be automatically dropped here
                        res.map_err(IoError::Io)
                    };
                    sync_completions_to_add.push((io_token, op, result));
                }
                Op::WriteFile { fd, offset, data } => {
                    let result = {
                        use std::os::unix::io::FromRawFd;
                        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
                        let res = file
                            .seek(SeekFrom::Start(offset))
                            .and_then(|_| file.write(&data))
                            .map(|bytes_written| CompletionKind::WriteFile { bytes_written });
                        // File will be automatically dropped here
                        data.recycle();
                        res.map_err(IoError::Io)
                    };
                    sync_completions_to_add.push((io_token, op, result));
                }
                Op::Fsync { fd } => {
                    let result = {
                        use std::os::unix::io::FromRawFd;
                        let file = unsafe { std::fs::File::from_raw_fd(fd) };
                        let res = file.sync_all();
                        // File will be automatically dropped here
                        res.map(|_| CompletionKind::Fsync).map_err(IoError::Io)
                    };
                    sync_completions_to_add.push((io_token, op, result));
                }
                Op::Close { fd } => {
                    let result = {
                        let res = unsafe { libc::close(fd) };
                        if res == -1 {
                            Err(IoError::Io(io::Error::last_os_error()))
                        } else {
                            Ok(CompletionKind::Close)
                        }
                    };
                    sync_completions_to_add.push((io_token, op, result));
                }
                _ => {
                    // Re-insert operations that were not handled by event loop
                    pending_ops.insert(mio_token, (io_token, op));
                }
            }
        }
        completions.extend(sync_completions_to_add);

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
                let sockaddr_in = &*(storage as *const _ as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sockaddr_in.sin_addr.s_addr));
                let port = u16::from_be(sockaddr_in.sin_port);
                std::net::SocketAddr::V4(std::net::SocketAddrV4::new(ip, port))
            }
            libc::AF_INET6 => {
                let sockaddr_in6 = &*(storage as *const _ as *const libc::sockaddr_in6);
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
