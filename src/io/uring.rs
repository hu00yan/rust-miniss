#![cfg(all(target_os = "linux", feature = "io_uring"))] // Only compile when io_uring feature is enabled

//! A `io-uring` backend for the I/O subsystem.

use crate::buffer::Buffer;
use crate::io::{CompletionKind, IoError, IoProvider, IoToken, Op};
use io_uring::{cqueue, opcode, types, IoUring};
use libc::{iovec, msghdr, sockaddr_storage, socklen_t};
use slab::Slab;
use std::cell::UnsafeCell;
use std::io;
use std::task::{Context, Poll};
use std::time::Duration;

/// A struct to hold a pending operation and any associated data, like buffers.
#[derive(Clone)]
enum PendingOp {
    Read {
        op: Op,
        buf: Buffer,
    },
    ReadVectored {
        op: Op,
        buffers: Vec<Buffer>,
        iovecs: Vec<iovec>,
    },
    WriteVectored {
        op: Op,
        buffers: Vec<Buffer>,
        iovecs: Vec<iovec>,
    },
    Write {
        op: Op,
        data: Buffer,
    },
    // ... existing UdpRecv/UdpSend extended for multi
    UdpRecv {
        op: Op,
        buffer: Buffer,
        _iov: Box<iovec>,
        _msg: Box<msghdr>,
        addr_storage: Box<sockaddr_storage>,
        addr_len: Box<socklen_t>,
    },
    UdpSend {
        op: Op,
        data: Buffer,
        _iov: Box<iovec>,
        _msg: Box<msghdr>,
        addr_storage: Box<sockaddr_storage>,
        addr_len: Box<socklen_t>,
    },
    Connect {
        op: Op,
        addr_storage: Box<sockaddr_storage>,
    },
    Accept {
        op: Op,
        addr_storage: Box<sockaddr_storage>,
        addr_len: Box<socklen_t>,
    },
    Fsync {
        op: Op,
    },
    Close {
        op: Op,
    },
    ReadFile {
        op: Op,
        buf: Buffer,
    },
    WriteFile {
        op: Op,
        data: Buffer,
    },
    PollAdd {
        op: Op,
    },
    /// Write got EAGAIN — waiting for POLLOUT before retry.
    WriteEagain {
        fd: i32,
        data: Buffer,
        orig_token: u64,
    },
    Nop {
        op: Op,
    },
}

/// A `io-uring` based `IoProvider`.
/// Thread-local to ensure single-threaded access.
pub struct UringBackend {
    ring: UnsafeCell<IoUring>,
    pending_ops: UnsafeCell<Slab<PendingOp>>,
    completed_ops: UnsafeCell<Vec<(IoToken, Op, Result<CompletionKind, IoError>)>>,
    thread_id: std::thread::ThreadId,
}

// SAFETY: This is safe in our thread-per-core model.
unsafe impl Send for UringBackend {}
unsafe impl Sync for UringBackend {}

impl UringBackend {
    pub fn new(entries: u32) -> io::Result<Self> {
        let ring = IoUring::builder()
            .setup_single_issuer()
            .setup_defer_taskrun()
            .setup_coop_taskrun()
            .build(entries)?;
        Ok(Self {
            ring: UnsafeCell::new(ring),
            // Use at least 64 entries for slab capacity
            pending_ops: UnsafeCell::new(Slab::with_capacity(std::cmp::max(entries, 64) as usize)),
            completed_ops: UnsafeCell::new(Vec::new()),
            thread_id: std::thread::current().id(),
        })
    }
}

impl IoProvider for UringBackend {
    type Completion = (IoToken, Op, Result<CompletionKind, IoError>);

    fn submit(&self, op: Op) -> IoToken {
        // Ensure single-threaded use
        assert_eq!(
            std::thread::current().id(),
            self.thread_id,
            "UringBackend must be used on the same thread"
        );

        // SAFETY: We have exclusive access on this thread.
        let ring = unsafe { &mut *self.ring.get() };
        let pending_ops = unsafe { &mut *self.pending_ops.get() };

        // Build the SQE entry and PendingOp WITHOUT user_data — we set it after
        // inserting into the slab to get the index.
        let (entry, pending_op) = {
            match op {
                Op::Accept { fd } => {
                    let mut addr_storage =
                        Box::new(unsafe { std::mem::zeroed::<sockaddr_storage>() });
                    let mut addr_len =
                        Box::new(std::mem::size_of::<sockaddr_storage>() as socklen_t);
                    let entry = opcode::Accept::new(
                        types::Fd(fd),
                        addr_storage.as_mut() as *mut _ as *mut _,
                        &mut *addr_len as *mut _,
                    )
                    .build();
                    (
                        entry,
                        PendingOp::Accept {
                            op: Op::Accept { fd },
                            addr_storage,
                            addr_len,
                        },
                    )
                }
                Op::Read { fd, offset, len } => {
                    let mut buf = Buffer::new_zeroed(len);
                    let entry = opcode::Read::new(types::Fd(fd), buf.as_mut_ptr(), len as u32)
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
                Op::Write { fd, offset, data } => {
                    let entry = opcode::Write::new(types::Fd(fd), data.as_ptr(), data.len() as u32)
                        .offset(offset)
                        .build();
                    (
                        entry,
                        PendingOp::Write {
                            op: Op::Write {
                                fd,
                                offset,
                                data: Buffer::empty(),
                            },
                            data,
                        },
                    )
                }
                Op::WriteFile { fd, offset, data } => {
                    let entry = opcode::Write::new(types::Fd(fd), data.as_ptr(), data.len() as u32)
                        .offset(offset)
                        .build();
                    (
                        entry,
                        PendingOp::WriteFile {
                            op: Op::WriteFile {
                                fd,
                                offset,
                                data: Buffer::empty(),
                            },
                            data,
                        },
                    )
                }
                Op::Fsync { fd } => {
                    let entry = opcode::Fsync::new(types::Fd(fd)).build();
                    (
                        entry,
                        PendingOp::Fsync {
                            op: Op::Fsync { fd },
                        },
                    )
                }
                Op::Close { fd } => {
                    let entry = opcode::Close::new(types::Fd(fd)).build();
                    (
                        entry,
                        PendingOp::Close {
                            op: Op::Close { fd },
                        },
                    )
                }
                Op::ReadFile { fd, offset, len } => {
                    let mut buf = Buffer::new_zeroed(len);
                    let entry = opcode::Read::new(types::Fd(fd), buf.as_mut_ptr(), len as u32)
                        .offset(offset)
                        .build();
                    (
                        entry,
                        PendingOp::ReadFile {
                            op: Op::ReadFile { fd, offset, len },
                            buf,
                        },
                    )
                }
                Op::UdpRecv { fd, mut buffer } => {
                    let mut iov = Box::new(iovec {
                        iov_base: buffer.as_mut_ptr() as *mut _,
                        iov_len: buffer.len(),
                    });
                    let mut addr_storage =
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

                    let entry = opcode::RecvMsg::new(types::Fd(fd), msg.as_mut() as *mut _).build();

                    // Op buffer is a placeholder — real buffer in PendingOp
                    let op = Op::UdpRecv {
                        fd,
                        buffer: Buffer::empty(),
                    };

                    (
                        entry,
                        PendingOp::UdpRecv {
                            op,
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

                    let entry = opcode::SendMsg::new(types::Fd(fd), msg.as_mut() as *mut _).build();

                    let op = Op::UdpSend {
                        fd,
                        data: Buffer::empty(),
                        addr,
                    };

                    (
                        entry,
                        PendingOp::UdpSend {
                            op,
                            data,
                            _iov: iov,
                            _msg: msg,
                            addr_storage,
                            addr_len,
                        },
                    )
                }
                Op::ReadVectored { fd, mut buffers } => {
                    // Take ownership of buffers for PendingOp; Op gets empty placeholder
                    let mut pending_buffers = std::mem::take(&mut buffers);
                    let iovecs: Vec<iovec> = pending_buffers
                        .iter_mut()
                        .map(|buf| iovec {
                            iov_base: buf.as_mut_ptr() as *mut _,
                            iov_len: buf.capacity(),
                        })
                        .collect();
                    let entry = opcode::Readv::new(
                        types::Fd(fd),
                        iovecs.as_ptr() as *const iovec,
                        iovecs.len() as u32,
                    )
                    .build();
                    let pending_op = PendingOp::ReadVectored {
                        op: Op::ReadVectored {
                            fd,
                            buffers: Vec::new(),
                        },
                        buffers: pending_buffers,
                        iovecs,
                    };
                    (entry, pending_op)
                }
                Op::WriteVectored { fd, buffers } => {
                    let iovecs: Vec<iovec> = buffers
                        .iter()
                        .map(|buf| iovec {
                            iov_base: buf.as_ptr() as *mut _,
                            iov_len: buf.len(),
                        })
                        .collect();
                    let entry = opcode::Writev::new(
                        types::Fd(fd),
                        iovecs.as_ptr() as *const iovec,
                        iovecs.len() as u32,
                    )
                    .build();
                    let pending_op = PendingOp::WriteVectored {
                        op: Op::WriteVectored {
                            fd,
                            buffers: Vec::new(),
                        },
                        buffers,
                        iovecs,
                    };
                    (entry, pending_op)
                }
                Op::Connect { fd, addr } => {
                    let mut addr_storage: Box<sockaddr_storage> =
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
                            unsafe {
                                std::ptr::write(
                                    addr_storage.as_mut() as *mut _ as *mut libc::sockaddr_in6,
                                    sin6,
                                );
                            }
                        }
                    }
                    let entry = opcode::Connect::new(
                        types::Fd(fd),
                        addr_storage.as_ref() as *const _ as *const libc::sockaddr,
                        addr_len,
                    )
                    .build();
                    (
                        entry,
                        PendingOp::Connect {
                            op: Op::Connect { fd, addr },
                            addr_storage,
                        },
                    )
                }
                Op::ReadRent { fd, buffer } => {
                    let mut buf = buffer;
                    let entry =
                        opcode::Read::new(types::Fd(fd), buf.as_mut_ptr(), buf.len() as u32)
                            .offset(0)
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
                    let entry = opcode::Write::new(types::Fd(fd), data.as_ptr(), data.len() as u32)
                        .offset(0)
                        .build();
                    (
                        entry,
                        PendingOp::Write {
                            // Op::WriteRent's data field is just a placeholder —
                            // the real buffer lives in PendingOp::Write.data and
                            // is returned to the caller on completion.
                            op: Op::WriteRent {
                                fd,
                                data: Buffer::empty(),
                            },
                            data,
                        },
                    )
                }
                Op::PollAdd {
                    fd,
                    events,
                    multishot,
                } => {
                    let mut entry = opcode::PollAdd::new(types::Fd(fd), events);
                    if multishot {
                        entry = entry.multi(true);
                    }
                    let entry = entry.build();
                    (
                        entry,
                        PendingOp::PollAdd {
                            op: Op::PollAdd {
                                fd,
                                events,
                                multishot,
                            },
                        },
                    )
                }
                Op::Nop => {
                    let entry = opcode::Nop::new().build();
                    (entry, PendingOp::Nop { op: Op::Nop })
                }
                _ => {
                    let result = Err(IoError::Io(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Operation not supported by io_uring backend",
                    )));
                    let completed_ops = unsafe { &mut *self.completed_ops.get() };
                    // Use a dummy token that will never match any waker — these
                    // unsupported ops will be returned directly.
                    let dummy_token = IoToken::from_id(u64::MAX);
                    completed_ops.push((dummy_token, op, result));
                    return dummy_token;
                }
            }
        };

        // Insert into slab to get index, use index as user_data (zero-alloc O(1) lookup)
        let idx = pending_ops.insert(pending_op);
        let token = IoToken::from_id(idx as u64);
        let entry = entry.user_data(idx as u64);

        // Proactive flush when SQ is filling up — avoids emergency flushes
        // that would break batch semantics. Thresholds: >75% capacity or >128 entries.
        let should_flush = {
            // submission() borrows ring mutably; scope it so submit() can borrow later
            let sq = ring.submission();
            sq.len() > (sq.capacity() * 3 / 4) || sq.len() > 128
        };
        if should_flush {
            let _ = ring.submit();
        }

        // Push to submission queue. Single push, no retry — proactive flush above
        // should keep SQ from ever being completely full.
        if unsafe { ring.submission().push(&entry) }.is_err() {
            pending_ops.try_remove(idx);
            eprintln!("SQ full, dropping op (ring too small?)");
        }

        // NO ring.submit() here — batch submission happens in flush() at event loop boundary
        token
    }

    fn flush(&self) {
        let ring = unsafe { &mut *self.ring.get() };
        // submit() returns io::Result<u32> with the number of submitted SQEs.
        // If SQ is empty, this is a fast no-op syscall.
        let _ = ring.submit();
    }

    fn park(&self, _timeout: Duration) -> usize {
        let ring = unsafe { &mut *self.ring.get() };
        // submit_and_wait(1) submits all pending SQEs and blocks until
        // at least 1 CQE is available. One syscall instead of submit + poll.
        match ring.submit_and_wait(1) {
            Ok(_) => {
                // Return number of available CQEs for the caller
                let cq = ring.completion();
                cq.len()
            }
            Err(_) => 0,
        }
    }

    fn poll_complete(&self, _cx: &mut Context<'_>) -> Poll<Vec<Self::Completion>> {
        let ring = unsafe { &mut *self.ring.get() };
        let pending_ops = unsafe { &mut *self.pending_ops.get() };
        let completed_ops = unsafe { &mut *self.completed_ops.get() };

        let mut completions: Vec<Self::Completion> = completed_ops.drain(..).collect();

        // Collect CQEs first to avoid borrow conflicts with ring.submission()
        // in the EAGAIN retry path below.
        let mut cq = ring.completion();
        cq.sync();
        let cqes: Vec<_> = cq.into_iter().collect();

        for cqe in &cqes {
            let token_id = cqe.user_data();
            let result = cqe.result();
            let flags = cqe.flags();
            let has_more = cqueue::more(flags);

            // ---- EAGAIN handling for Write ops ----
            // When a Write on a TCP socket returns -EAGAIN, the send buffer is
            // full. We keep the PendingOp alive, submit POLLOUT on the fd.
            // When POLLOUT fires, we re-submit the write. Throughout this
            // process, we return WriteRetryWaiting completions that tell poll_io
            // to transfer the waker/pending_task from the old token to the new,
            // ensuring the task is woken when the retried write completes.
            if !has_more
                && token_id != u64::MAX
                && result == -libc::EAGAIN
                && pending_ops
                    .get(token_id as usize)
                    .is_some_and(|op| matches!(op, PendingOp::Write { .. }))
            {
                let (fd, data) = {
                    let entry = pending_ops.try_remove(token_id as usize).unwrap();
                    match entry {
                        PendingOp::Write { op, data } => {
                            let fd = match op {
                                Op::Write { fd, .. } | Op::WriteRent { fd, .. } => fd,
                                _ => unreachable!(),
                            };
                            (fd, data)
                        }
                        _ => unreachable!(),
                    }
                }; // pending_ops mutable borrow ends here

                // Replace with WriteEagain state — holds the buffer alive
                let eagain_idx = pending_ops.insert(PendingOp::WriteEagain {
                    fd,
                    data,
                    orig_token: token_id,
                });

                // Submit POLLOUT on the fd
                let poll_entry = opcode::PollAdd::new(types::Fd(fd), libc::POLLOUT as u32)
                    .build()
                    .user_data(eagain_idx as u64);
                if unsafe { ring.submission().push(&poll_entry) }.is_err() {
                    // SQ full — will retry when next CQE arrives
                }

                // Return WriteRetryWaiting so poll_io transfers waker
                completions.push((
                    IoToken::from_id(token_id),
                    Op::PollAdd {
                        fd,
                        events: libc::POLLOUT as u32,
                        multishot: false,
                    },
                    Ok(CompletionKind::WriteRetryWaiting {
                        new_token: IoToken::from_id(eagain_idx as u64),
                        fd,
                    }),
                ));
                continue;
            }

            // ---- POLLOUT firing for a WriteEagain retry ----
            // The PendingOp at this slot is WriteEagain, meaning POLLOUT
            // has fired and the socket is writable. Re-submit the write.
            if !has_more
                && token_id != u64::MAX
                && result >= 0
                && pending_ops
                    .get(token_id as usize)
                    .is_some_and(|op| matches!(op, PendingOp::WriteEagain { .. }))
            {
                let (fd, data, _orig_token) = {
                    let entry = pending_ops.try_remove(token_id as usize).unwrap();
                    match entry {
                        PendingOp::WriteEagain {
                            fd,
                            data,
                            orig_token,
                        } => (fd, data, orig_token),
                        _ => unreachable!(),
                    }
                };

                // Capture buffer info before slab insert
                let ptr = data.as_ptr();
                let len = data.len();

                // Re-submit the write — PendingOp owns the buffer
                let write_idx = pending_ops.insert(PendingOp::Write {
                    op: Op::Write {
                        fd,
                        offset: 0,
                        data: Buffer::empty(),
                    },
                    data,
                });
                let entry = opcode::Write::new(types::Fd(fd), ptr as *mut _, len as u32)
                    .offset(0)
                    .build()
                    .user_data(write_idx as u64);
                if unsafe { ring.submission().push(&entry) }.is_err() {
                    // SQ full — will retry
                }

                // Tell poll_io to transfer waker from POLLOUT token to new write token
                completions.push((
                    IoToken::from_id(token_id),
                    Op::PollAdd {
                        fd,
                        events: libc::POLLOUT as u32,
                        multishot: false,
                    },
                    Ok(CompletionKind::WriteRetryWaiting {
                        new_token: IoToken::from_id(write_idx as u64),
                        fd,
                    }),
                ));
                continue;
            }

            // Normal completion processing
            let pending_op = if has_more && token_id != u64::MAX {
                pending_ops.get(token_id as usize).cloned()
            } else {
                pending_ops.try_remove(token_id as usize)
            };

            if let Some(pending_op) = pending_op {
                // Reconstruct original token from CQE user_data
                let token = IoToken::from_id(token_id);

                let (op, completion_result) = match pending_op {
                    PendingOp::Accept {
                        op,
                        addr_storage,
                        addr_len,
                    } => {
                        let res = if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            // convert sockaddr_storage to SocketAddr
                            let socket_addr = {
                                let storage: *const libc::sockaddr_storage =
                                    &*addr_storage as *const _;
                                unsafe {
                                    match (*storage).ss_family as libc::c_int {
                                        libc::AF_INET => {
                                            let sockaddr_in =
                                                &*(storage as *const _ as *const libc::sockaddr_in);
                                            let ip = std::net::Ipv4Addr::from(u32::from_be(
                                                sockaddr_in.sin_addr.s_addr,
                                            ));
                                            let port = u16::from_be(sockaddr_in.sin_port);
                                            Ok(std::net::SocketAddr::V4(
                                                std::net::SocketAddrV4::new(ip, port),
                                            ))
                                        }
                                        libc::AF_INET6 => {
                                            let sockaddr_in6 = &*(storage as *const _
                                                as *const libc::sockaddr_in6);
                                            let ip = std::net::Ipv6Addr::from(
                                                sockaddr_in6.sin6_addr.s6_addr,
                                            );
                                            let port = u16::from_be(sockaddr_in6.sin6_port);
                                            Ok(std::net::SocketAddr::V6(
                                                std::net::SocketAddrV6::new(
                                                    ip,
                                                    port,
                                                    sockaddr_in6.sin6_flowinfo,
                                                    sockaddr_in6.sin6_scope_id,
                                                ),
                                            ))
                                        }
                                        _ => Err(IoError::Io(io::Error::new(
                                            io::ErrorKind::InvalidData,
                                            "Unsupported address family",
                                        ))),
                                    }
                                }
                            };
                            // touch addr_len so it's considered read (we keep it to keep memory alive)
                            let _ = *addr_len;
                            match socket_addr {
                                Ok(sa) => Ok(CompletionKind::Accept {
                                    fd: result,
                                    addr: Some(sa),
                                }),
                                Err(e) => Err(e),
                            }
                        };
                        (op, res)
                    }
                    PendingOp::Read { op, mut buf } => {
                        let res = if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            let bytes_read = result as usize;
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
                        };
                        (op, res)
                    }
                    PendingOp::Write { op, data } => {
                        let res = {
                            if result < 0 {
                                // Error: data is dropped and recycled to pool
                                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                            } else {
                                match &op {
                                    Op::WriteRent { .. } => {
                                        // Return the buffer to the caller for reuse
                                        Ok(CompletionKind::WriteRent {
                                            bytes_written: result as usize,
                                            data,
                                        })
                                    }
                                    _ => {
                                        // Non-rent write: data is recycled via Buffer::Drop
                                        let _ = data;
                                        Ok(CompletionKind::Write {
                                            bytes_written: result as usize,
                                        })
                                    }
                                }
                            }
                        };
                        (op, res)
                    }
                    PendingOp::Fsync { op } => {
                        let res = if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::Fsync)
                        };
                        (op, res)
                    }
                    PendingOp::Close { op } => {
                        let res = if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::Close)
                        };
                        (op, res)
                    }
                    PendingOp::ReadFile { op, mut buf } => {
                        let res = if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            let bytes_read = result as usize;
                            unsafe {
                                buf.set_len(bytes_read);
                            }
                            Ok(CompletionKind::ReadFile {
                                bytes_read,
                                data: buf,
                            })
                        };
                        (op, res)
                    }
                    PendingOp::WriteFile { op, data } => {
                        // Drop data (recycles to pool) — kept alive until
                        // completion so the kernel can read/write it.
                        let _ = data;
                        let res = {
                            if result < 0 {
                                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                            } else {
                                Ok(CompletionKind::WriteFile {
                                    bytes_written: result as usize,
                                })
                            }
                        };
                        (op, res)
                    }
                    PendingOp::UdpRecv {
                        op,
                        mut buffer,
                        addr_storage,
                        addr_len,
                        ..
                    } => {
                        let res = if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            let bytes_read = result as usize;
                            unsafe {
                                buffer.set_len(bytes_read);
                            }
                            // convert sockaddr_storage to SocketAddr
                            let socket_addr = {
                                let storage: *const libc::sockaddr_storage =
                                    &*addr_storage as *const _;
                                unsafe {
                                    match (*storage).ss_family as libc::c_int {
                                        libc::AF_INET => {
                                            let sockaddr_in =
                                                &*(storage as *const _ as *const libc::sockaddr_in);
                                            let ip = std::net::Ipv4Addr::from(u32::from_be(
                                                sockaddr_in.sin_addr.s_addr,
                                            ));
                                            let port = u16::from_be(sockaddr_in.sin_port);
                                            Ok(std::net::SocketAddr::V4(
                                                std::net::SocketAddrV4::new(ip, port),
                                            ))
                                        }
                                        libc::AF_INET6 => {
                                            let sockaddr_in6 = &*(storage as *const _
                                                as *const libc::sockaddr_in6);
                                            let ip = std::net::Ipv6Addr::from(
                                                sockaddr_in6.sin6_addr.s6_addr,
                                            );
                                            let port = u16::from_be(sockaddr_in6.sin6_port);
                                            Ok(std::net::SocketAddr::V6(
                                                std::net::SocketAddrV6::new(
                                                    ip,
                                                    port,
                                                    sockaddr_in6.sin6_flowinfo,
                                                    sockaddr_in6.sin6_scope_id,
                                                ),
                                            ))
                                        }
                                        _ => Err(IoError::Io(io::Error::new(
                                            io::ErrorKind::InvalidData,
                                            "Unsupported address family",
                                        ))),
                                    }
                                }
                            };
                            // read addr_len to avoid unused-field warning
                            let _ = *addr_len;
                            match socket_addr {
                                Ok(sa) => Ok(CompletionKind::UdpRecv {
                                    bytes_read,
                                    buffer,
                                    addr: sa,
                                }),
                                Err(e) => Err(e),
                            }
                        };
                        (op, res)
                    }
                    PendingOp::UdpSend {
                        op,
                        data,
                        addr_storage,
                        addr_len,
                        ..
                    } => {
                        // touch addr_storage/addr_len so they're considered read (keeps them alive)
                        let _ = &*addr_storage as *const _;
                        let _ = *addr_len;
                        let res = if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::UdpSend {
                                bytes_written: result as usize,
                                data,
                            })
                        };
                        (op, res)
                    }
                    PendingOp::Connect { op, addr_storage } => {
                        let res = if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            let _ = &*addr_storage;
                            Ok(CompletionKind::Connect)
                        };
                        (op, res)
                    }
                    PendingOp::ReadVectored {
                        op,
                        mut buffers,
                        iovecs,
                    } => {
                        // Keep iovecs alive until buffers are no longer needed
                        let _ = iovecs;
                        let res = if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            let bytes_read = result as usize;
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
                            Ok(CompletionKind::ReadVectored {
                                bytes_read,
                                buffers,
                            })
                        };
                        (op, res)
                    }
                    PendingOp::WriteVectored {
                        op,
                        buffers,
                        iovecs,
                    } => {
                        // Keep buffers and iovecs alive until completion
                        let _ = buffers;
                        let _ = iovecs;
                        let res = if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::WriteVectored {
                                bytes_written: result as usize,
                            })
                        };
                        (op, res)
                    }
                    PendingOp::PollAdd { op } => {
                        let res = if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::PollAdd {
                                events: result as u32,
                            })
                        };
                        (op, res)
                    }
                    // WriteEagain entries are caught before the main match
                    // in the EAGAIN/POLLOUT retry handling above, but the
                    // compiler needs this arm for exhaustiveness.
                    PendingOp::Nop { op } => {
                        let res = if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::Nop)
                        };
                        (op, res)
                    }
                    PendingOp::WriteEagain { .. } => {
                        unreachable!("WriteEagain should be handled before the main match")
                    }
                };
                completions.push((token, op, completion_result));
            }
        }

        if completions.is_empty() {
            Poll::Pending
        } else {
            Poll::Ready(completions)
        }
    }
}
