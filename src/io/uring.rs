#![cfg(all(target_os = "linux", feature = "io_uring"))] // Only compile when io_uring feature is enabled

//! A `io-uring` backend for the I/O subsystem.

use crate::buffer::Buffer;
use crate::io::{CompletionKind, IoError, IoProvider, IoToken, Op};
use io_uring::{opcode, types, IoUring}; // Import opcode, types and IoUring directly
use libc::{iovec, msghdr, sockaddr_storage, socklen_t};
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::io;
use std::task::{Context, Poll};

/// A struct to hold a pending operation and any associated data, like buffers.
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
        _data: Buffer,
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
        _data: Buffer,
    },
}

/// A `io-uring` based `IoProvider`.
/// Thread-local to ensure single-threaded access.
pub struct UringBackend {
    ring: UnsafeCell<IoUring>,
    pending_ops: UnsafeCell<HashMap<u64, PendingOp>>,
    thread_id: std::thread::ThreadId,
}

// SAFETY: This is safe in our thread-per-core model.
unsafe impl Send for UringBackend {}
unsafe impl Sync for UringBackend {}

impl UringBackend {
    pub fn new(entries: u32) -> io::Result<Self> {
        let ring = IoUring::new(entries)?;
        Ok(Self {
            ring: UnsafeCell::new(ring),
            pending_ops: UnsafeCell::new(HashMap::new()),
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

        let token = IoToken::new();
        let user_data = token.id();

        // SAFETY: We have exclusive access on this thread.
        let ring = unsafe { &mut *self.ring.get() };
        let pending_ops = unsafe { &mut *self.pending_ops.get() };

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
                    .build()
                    .user_data(user_data);
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
                        .build()
                        .user_data(user_data);
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
                        .build()
                        .user_data(user_data);
                    (
                        entry,
                        PendingOp::Write {
                            op: Op::Write {
                                fd,
                                offset,
                                data: data.clone(), // Clone data for op, move original into PendingOp
                            },
                            _data: data, // Store the original Buffer here
                        },
                    )
                }
                Op::Fsync { fd } => {
                    let entry = opcode::Fsync::new(types::Fd(fd))
                        .build()
                        .user_data(user_data);
                    (
                        entry,
                        PendingOp::Fsync {
                            op: Op::Fsync { fd },
                        },
                    )
                }
                Op::Close { fd } => {
                    let entry = opcode::Close::new(types::Fd(fd))
                        .build()
                        .user_data(user_data);
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
                        .build()
                        .user_data(user_data);
                    (
                        entry,
                        PendingOp::ReadFile {
                            op: Op::ReadFile { fd, offset, len },
                            buf,
                        },
                    )
                }
                Op::WriteFile { fd, offset, data } => {
                    // The data Buffer is moved into PendingOp to ensure it lives until completion.
                    let entry = opcode::Write::new(types::Fd(fd), data.as_ptr(), data.len() as u32)
                        .offset(offset)
                        .build()
                        .user_data(user_data);
                    (
                        entry,
                        PendingOp::WriteFile {
                            op: Op::WriteFile {
                                fd,
                                offset,
                                data: data.clone(), // Clone data for op, move original into PendingOp
                            },
                            _data: data, // Store the original Buffer here
                        },
                    )
                }
                Op::UdpRecv { fd, mut buffer } => {
                    let buffer_clone = buffer.clone();
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

                    let entry = opcode::RecvMsg::new(types::Fd(fd), msg.as_mut() as *mut _)
                        .build()
                        .user_data(user_data);

                    let op = Op::UdpRecv {
                        fd,
                        buffer: buffer_clone,
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
                    let data_clone = data.clone();
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

                    let entry = opcode::SendMsg::new(types::Fd(fd), msg.as_mut() as *mut _)
                        .build()
                        .user_data(user_data);

                    let op = Op::UdpSend {
                        fd,
                        data: data_clone,
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
                Op::ReadVectored { fd, buffers } => {
                    let mut pending_buffers = buffers.clone();
                    let iovecs: Vec<iovec> = pending_buffers
                        .iter_mut()
                        .map(|buf| iovec {
                            iov_base: buf.as_mut_ptr() as *mut _,
                            iov_len: buf.capacity(),
                        })
                        .collect();
                    let iovecs_ptr = iovecs.as_ptr();
                    let iovecs_len = iovecs.len();
                    let entry = unsafe {
                        opcode::Readv::new(
                            types::Fd(fd),
                            iovecs_ptr as *const iovec,
                            iovecs_len as u32,
                        )
                        .build()
                        .user_data(user_data);
                    };
                    let pending_op = PendingOp::ReadVectored {
                        op: Op::ReadVectored { fd, buffers },
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
                    let iovecs_ptr = iovecs.as_ptr();
                    let iovecs_len = iovecs.len();
                    let entry = unsafe {
                        opcode::Writev::new(
                            types::Fd(fd),
                            iovecs_ptr as *const iovec,
                            iovecs_len as u32,
                        )
                        .build()
                        .user_data(user_data);
                    };
                    let pending_op = PendingOp::WriteVectored {
                        op: Op::WriteVectored { fd, buffers: buffers.clone() },
                        buffers,
                        iovecs,
                    };
                    (entry, pending_op)
                }
                _ => {
                    let result = Err(IoError::Io(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Operation not supported by io_uring backend",
                    )));
                    eprintln!("Unsupported io_uring operation: {result:?}");
                    // Return token without submitting — caller will get no completion.
                    // This is a graceful degradation for operations like UdpRecvVectored,
                    // UdpSendVectored, ReadRent, WriteRent that aren't implemented yet.
                    return token;
                }
            }
        };

        match unsafe { ring.submission().push(&entry) } {
            Ok(_) => {
                pending_ops.insert(user_data, pending_op);
            }
            Err(e) => {
                eprintln!("Failed to submit io-uring operation: {e}");
            }
        }

        let _ = ring.submit();
        token
    }

    fn poll_complete(&self, _cx: &mut Context<'_>) -> Poll<Vec<Self::Completion>> {
        let ring = unsafe { &mut *self.ring.get() };
        let pending_ops = unsafe { &mut *self.pending_ops.get() };

        let mut completions = Vec::new();
        let mut cq = ring.completion();
        cq.sync();

        for cqe in cq {
            let token_id = cqe.user_data();
            let result = cqe.result();

            if let Some(pending_op) = pending_ops.remove(&token_id) {
                let token = IoToken {
                    id: token_id,
                    slab_index: 0,
                }; // TODO: proper slab index

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
                            Ok(CompletionKind::Read {
                                bytes_read,
                                data: buf,
                            })
                        };
                        (op, res)
                    }
                    PendingOp::Write { op, _data: _ } => {
                        let res = {
                            if result < 0 {
                                Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                            } else {
                                Ok(CompletionKind::Write {
                                    bytes_written: result as usize,
                                })
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
                    PendingOp::WriteFile { op, _data: _ } => {
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
                    PendingOp::ReadVectored { op, mut buffers, .. } => {
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
                    PendingOp::WriteVectored { op, .. } => {
                        let res = if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::WriteVectored {
                                bytes_written: result as usize,
                            })
                        };
                        (op, res)
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
