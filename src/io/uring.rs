#![cfg(all(target_os = "linux", io_backend = "io_uring"))] // Only compile when io_uring backend is selected

//! A `io-uring` backend for the I/O subsystem.

use crate::buffer::Buffer;
use crate::io::{CompletionKind, IoError, IoProvider, IoToken, Op};
use io_uring::{cqueue, opcode, types, IoUring};
use libc::{iovec, msghdr, sockaddr_storage, socklen_t};
use slab::Slab;
use smallvec::SmallVec;
use std::cell::{Cell, UnsafeCell};
use std::collections::VecDeque;
use std::io;
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
        op: Op,
        buffers: SmallVec<[Buffer; 4]>,
        /// Kernel-visible iovec metadata in immovable heap storage.
        /// Must be boxed *before* SQE construction so `as_ptr()` remains valid
        /// across moves into this enum and into the pending-ops slab.
        iovecs: Box<[iovec]>,
    },
    WriteVectored {
        op: Op,
        buffers: SmallVec<[Buffer; 4]>,
        /// Kernel-visible iovec metadata in immovable heap storage.
        /// See `ReadVectored::iovecs`.
        iovecs: Box<[iovec]>,
    },
    Write {
        op: Op,
        data: Buffer,
    },
    WriteStatic {
        op: Op,
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
        addr_storage: Option<Box<sockaddr_storage>>,
        addr_len: Option<Box<socklen_t>>,
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
    Nop {
        op: Op,
    },
}

#[derive(Clone, Copy, Default)]
struct SlotMeta {
    generation: u32,
    token_id: u64,
}

/// The completion type for the io_uring backend.
pub(super) type Completion = (IoToken, Result<CompletionKind, IoError>);
type AcceptResults = Vec<Option<VecDeque<Result<CompletionKind, IoError>>>>;

/// A `io-uring` based `IoProvider`.
/// Thread-local to ensure single-threaded access.
pub struct UringBackend {
    ring: UnsafeCell<IoUring>,
    pending_ops: UnsafeCell<Slab<PendingOp>>,
    /// Generation per slab slot. Packed with the slot index into CQE user_data
    /// so stale completions can be dropped without a hash table lookup.
    slot_meta: UnsafeCell<Vec<SlotMeta>>,
    /// Monotonic token counter for unique shard-local IoToken values.
    next_token_id: Cell<u64>,
    /// Fallback completion queue for unsupported ops and as a capacity hint
    /// for the reusable completion buffer.
    completed_ops: UnsafeCell<Vec<Completion>>,
    /// Multi-shot accept completions indexed by slab slot.
    accept_results: UnsafeCell<AcceptResults>,
    /// Count of SQEs not yet flushed. Zero means flush() can skip the syscall.
    pending_sqes: Cell<usize>,
    /// True when the kernel SQPOLL thread owns submission. In that mode flush()
    /// only publishes the SQ tail; it must not call io_uring_enter.
    sqpoll_enabled: bool,
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
    thread_id: std::thread::ThreadId,
}

// SAFETY: This is safe in our thread-per-core model.
unsafe impl Send for UringBackend {}
unsafe impl Sync for UringBackend {}

/// Minimum pending SQEs before flush() calls io_uring_enter submit.
/// Deferring small batches reduces syscall rate.
/// Kept below MAX_BUSY_ROUNDS in multicore.rs so busy workers still submit
/// without waiting for the outer park path.
const FLUSH_BATCH_SIZE: usize = 61;
/// Maximum CQEs processed in one drain pass when the runtime asks for a
/// larger sweep. The caller can still cap this lower on busy shards.
const MAX_COMPLETIONS_PER_DRAIN: usize = 256;

impl UringBackend {
    pub fn new(entries: u32) -> io::Result<Self> {
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
        fn base_builder() -> io_uring::Builder {
            let mut builder = IoUring::builder();
            // Match monoio's default posture: leave the builder mostly plain
            // unless a benchmark explicitly asks for extra flags.
            if std::env::var_os("MINISS_URING_SINGLE_ISSUER").is_some() {
                builder.setup_single_issuer();
            }
            if std::env::var_os("MINISS_URING_COOP_TASKRUN").is_some() {
                builder.setup_coop_taskrun();
            }
            if std::env::var_os("MINISS_URING_DEFER_TASKRUN").is_some() {
                builder.setup_defer_taskrun();
            }
            builder
        }

        let (ring, sqpoll_enabled) = if std::env::var_os("MINISS_URING_SQPOLL").is_some() {
            let mut builder = base_builder();
            builder.setup_sqpoll(1000);
            match builder.build(entries) {
                Ok(ring) => (ring, true),
                Err(err) => {
                    tracing::warn!("io_uring SQPOLL unavailable, falling back: {err}");
                    (base_builder().build(entries)?, false)
                }
            }
        } else {
            (base_builder().build(entries)?, false)
        };
        let ring_entries = entries as usize;
        let flush_high_watermark = std::cmp::max(1, ring_entries.saturating_mul(3) / 4);
        let capacity = std::cmp::max(entries, 64) as usize;
        let mut accept_results = Vec::with_capacity(capacity);
        accept_results.resize_with(capacity, || None);
        Ok(Self {
            ring: UnsafeCell::new(ring),
            // Use at least 64 entries for slab capacity
            pending_ops: UnsafeCell::new(Slab::with_capacity(capacity)),
            slot_meta: UnsafeCell::new(vec![SlotMeta::default(); capacity]),
            next_token_id: Cell::new(1),
            completed_ops: UnsafeCell::new(Vec::with_capacity(capacity)),
            accept_results: UnsafeCell::new(accept_results),
            pending_sqes: Cell::new(0),
            sqpoll_enabled,
            flush_high_watermark,
            flush_batch_size,
            drain_batch_size,
            park_flushes: Cell::new(0),
            park_flush_total_us: Cell::new(0),
            max_park_flush_us: Cell::new(0),
            thread_id: std::thread::current().id(),
        })
    }

    /// Submit all pending SQEs unconditionally.
    /// Used by flush() for batch threshold, park() before sleep, and submit() SQ-full.
    fn do_flush(&self) {
        if self.pending_sqes.get() == 0 {
            return;
        }
        // SAFETY: UringBackend is thread-affine; the owning runtime thread is
        // the only code path mutating the ring.
        let ring = unsafe { &mut *self.ring.get() };
        if self.sqpoll_enabled {
            ring.submission().sync();
            self.pending_sqes.set(0);
            return;
        }
        if ring.submit().is_ok() {
            self.pending_sqes.set(0);
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
        let slot_meta = &*self.slot_meta.get();
        // SAFETY: this method is itself restricted to the owning shard turn.
        let io_state = unsafe { crate::cpu::current_io_state_unchecked() };

        let mut cq = ring.completion();
        cq.sync();

        let mut processed = 0usize;
        let mut saw_wakeup_poll_completion = false;
        let mut wakeups = 0usize;
        for cqe in cq {
            let token_id = cqe.user_data();
            let result = cqe.result();
            let flags = cqe.flags();
            let has_more = cqueue::more(flags);

            if token_id == u64::MAX {
                continue;
            }
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
                continue;
            }

            // Multishot accept: stays armed, result goes into accept_results VecDeque.
            if has_more {
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
                        match socket_addr {
                            Ok(sa) => Ok(CompletionKind::Accept {
                                fd: result,
                                addr: Some(sa),
                            }),
                            Err(e) => Err(e),
                        }
                    } else {
                        Ok(CompletionKind::Accept {
                            fd: result,
                            addr: None,
                        })
                    };
                    let token_idx = token.slot_index();
                    let queue = accept_results[token_idx].get_or_insert_with(VecDeque::new);
                    queue.push_back(completion_result);
                    if let Some(waker) = io_state.take_waker(token) {
                        wakeups += 1;
                        waker.wake();
                    }
                    continue;
                }
            }

            // Normal completion: remove PendingOp, reconstruct result, store + wake.
            if let Some(pending_op) = pending_ops.try_remove(idx) {
                let completion_result = match pending_op {
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
                            let storage: *const sockaddr_storage = &**addr_storage as *const _;
                            let socket_addr = unsafe {
                                match (*storage).ss_family as libc::c_int {
                                    libc::AF_INET => {
                                        let sa =
                                            &*(storage as *const _ as *const libc::sockaddr_in);
                                        let ip = std::net::Ipv4Addr::from(u32::from_be(
                                            sa.sin_addr.s_addr,
                                        ));
                                        let port = u16::from_be(sa.sin_port);
                                        Ok(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                                            ip, port,
                                        )))
                                    }
                                    libc::AF_INET6 => {
                                        let sa =
                                            &*(storage as *const _ as *const libc::sockaddr_in6);
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
                            match socket_addr {
                                Ok(sa) => Ok(CompletionKind::Accept {
                                    fd: result,
                                    addr: Some(sa),
                                }),
                                Err(e) => Err(e),
                            }
                        } else {
                            Ok(CompletionKind::Accept {
                                fd: result,
                                addr: None,
                            })
                        }
                    }
                    PendingOp::Read { op, mut buf } => {
                        if result < 0 {
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
                        }
                    }
                    PendingOp::Write { op, data } => {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            match &op {
                                Op::WriteRent { .. } => Ok(CompletionKind::WriteRent {
                                    bytes_written: result as usize,
                                    data,
                                }),
                                _ => {
                                    let _ = data;
                                    Ok(CompletionKind::Write {
                                        bytes_written: result as usize,
                                    })
                                }
                            }
                        }
                    }
                    PendingOp::WriteStatic { .. } => {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::Write {
                                bytes_written: result as usize,
                            })
                        }
                    }
                    PendingOp::Fsync { .. } => {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::Fsync)
                        }
                    }
                    PendingOp::Close { .. } => {
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
                        let _ = data;
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::WriteFile {
                                bytes_written: result as usize,
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
                            unsafe {
                                buffer.set_len(bytes_read);
                            }
                            let socket_addr = {
                                let storage: *const sockaddr_storage = &*addr_storage as *const _;
                                unsafe {
                                    match (*storage).ss_family as libc::c_int {
                                        libc::AF_INET => {
                                            let sa =
                                                &*(storage as *const _ as *const libc::sockaddr_in);
                                            let ip = std::net::Ipv4Addr::from(u32::from_be(
                                                sa.sin_addr.s_addr,
                                            ));
                                            let port = u16::from_be(sa.sin_port);
                                            Ok(std::net::SocketAddr::V4(
                                                std::net::SocketAddrV4::new(ip, port),
                                            ))
                                        }
                                        libc::AF_INET6 => {
                                            let sa = &*(storage as *const _
                                                as *const libc::sockaddr_in6);
                                            let ip = std::net::Ipv6Addr::from(sa.sin6_addr.s6_addr);
                                            let port = u16::from_be(sa.sin6_port);
                                            Ok(std::net::SocketAddr::V6(
                                                std::net::SocketAddrV6::new(
                                                    ip,
                                                    port,
                                                    sa.sin6_flowinfo,
                                                    sa.sin6_scope_id,
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
                    PendingOp::WriteVectored {
                        buffers, iovecs, ..
                    } => {
                        let _ = iovecs;
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::WriteVectored {
                                bytes_written: result as usize,
                                buffers,
                            })
                        }
                    }
                    PendingOp::PollAdd { .. } => {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::PollAdd {
                                events: result as u32,
                            })
                        }
                    }
                    PendingOp::Nop { .. } => {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::Nop)
                        }
                    }
                };

                // Store result in slot and wake the task — no intermediate Vec.
                if let Some(task_id) =
                    io_state.take_pending_task_and_store_completed(token, completion_result)
                {
                    let _ = task_id;
                }
                if let Some(waker) = io_state.take_waker(token) {
                    wakeups += 1;
                    waker.wake();
                }
            }

            processed += 1;
            if processed >= self.drain_batch_size.min(batch_limit) {
                break;
            }
        }
        (processed, saw_wakeup_poll_completion, wakeups)
    }
}

impl IoProvider for UringBackend {
    type Completion = (IoToken, Result<CompletionKind, IoError>);

    fn submit(&self, op: Op) -> IoToken {
        // Ensure single-threaded use (debug builds only)
        debug_assert_eq!(
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
                    let entry = opcode::AcceptMulti::new(types::Fd(fd))
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
                }
                Op::Read { fd, offset, len } => {
                    let mut buf = Buffer::new_uninitialized(len);
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
                Op::WriteStatic { fd, data } => {
                    let entry = opcode::Write::new(types::Fd(fd), data.as_ptr(), data.len() as u32)
                        .offset(0)
                        .build();
                    (
                        entry,
                        PendingOp::WriteStatic {
                            op: Op::WriteStatic { fd, data },
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
                    let mut buf = Buffer::new_uninitialized(len);
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
                        iov_len: buffer.capacity(),
                    });
                    // SAFETY: sockaddr_storage is plain C storage used as an
                    // output buffer for recvmsg.
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

                    // SAFETY: sockaddr_storage is plain C storage and will be
                    // overwritten with the matching socket address.
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
                    let entry =
                        opcode::Readv::new(types::Fd(fd), iovecs.as_ptr(), iovecs.len() as u32)
                            .build();
                    let pending_op = PendingOp::ReadVectored {
                        op: Op::ReadVectored {
                            fd,
                            buffers: SmallVec::new(),
                        },
                        buffers: pending_buffers,
                        iovecs,
                    };
                    (entry, pending_op)
                }
                Op::WriteVectored { fd, buffers } => {
                    // CRITICAL (S0-4): same stable-boxed iovec pattern as ReadVectored.
                    // Build Box<[iovec]> first, then form the SQE from its as_ptr().
                    let iovecs: Box<[iovec]> = buffers
                        .iter()
                        .map(|buf| iovec {
                            iov_base: buf.as_ptr() as *mut _,
                            iov_len: buf.len(),
                        })
                        .collect();
                    let entry =
                        opcode::Writev::new(types::Fd(fd), iovecs.as_ptr(), iovecs.len() as u32)
                            .build();
                    let pending_op = PendingOp::WriteVectored {
                        op: Op::WriteVectored {
                            fd,
                            buffers: SmallVec::new(),
                        },
                        buffers,
                        iovecs,
                    };
                    (entry, pending_op)
                }
                Op::Connect { fd, addr } => {
                    // SAFETY: sockaddr_storage is plain C storage and will be
                    // overwritten with the matching socket address.
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
                        opcode::Read::new(types::Fd(fd), buf.as_mut_ptr(), buf.capacity() as u32)
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
                    // SAFETY: completed_ops is shard-local and mutated only by
                    // this UringBackend owner thread.
                    let completed_ops = unsafe { &mut *self.completed_ops.get() };
                    // Use a dummy token that will never match any waker — these
                    // unsupported ops will be returned directly.
                    let dummy_token = IoToken::from_id(u64::MAX);
                    completed_ops.push((dummy_token, result));
                    return dummy_token;
                }
            }
        };
        let is_accept = matches!(&pending_op, PendingOp::Accept { .. });

        // Insert into slab to get efficient O(1) storage.
        let idx = pending_ops.insert(pending_op);

        // SAFETY: slot_meta is shard-local and only
        // mutated by this backend owner thread while submitting/draining.
        let slot_meta = unsafe { &mut *self.slot_meta.get() };
        if idx >= slot_meta.len() {
            slot_meta.resize_with(idx + 1, SlotMeta::default);
        }
        let generation = slot_meta[idx].generation.wrapping_add(1).max(1);
        slot_meta[idx].generation = generation;
        let token = IoToken::next_from(&self.next_token_id);
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
        let entry = entry.user_data(Self::pack_slot(idx, generation));

        // Push to submission queue. If SQ is full, flush and retry.
        // NO proactive flush here — batch submission happens in flush() at event loop boundary.
        // SAFETY: `entry` lives until push copies it into the submission queue;
        // the ring is owned by this thread.
        if unsafe { ring.submission().push(&entry) }.is_err() {
            // SQ full: flush_all submits everything pending, freeing SQ slots.
            self.do_flush();
            // SAFETY: Same as above; retry after flushing free SQ slots.
            if unsafe { ring.submission().push(&entry) }.is_err() {
                // Still full after flush — ring is too small, drop the op.
                pending_ops.try_remove(idx);
                tracing::error!("SQ still full after emergency flush (ring too small?)");
            }
        } else {
            // Successful push — count it so flush() knows there's work.
            let pending_sqes = self.pending_sqes.get() + 1;
            self.pending_sqes.set(pending_sqes);
            if pending_sqes >= self.flush_high_watermark {
                self.do_flush();
            }
        }

        token
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
        self.do_flush();
    }

    fn pending_submission_count(&self) -> usize {
        self.pending_sqes.get()
    }

    fn park(&self, timeout: Duration) -> usize {
        // SAFETY: UringBackend is thread-affine; the owning runtime thread is
        // the only code path mutating the ring.
        let ring = unsafe { &mut *self.ring.get() };

        // Check if completions are already available.
        // No need to wait if results are ready.
        let ready = ring.completion().len();
        if ready != 0 {
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
        self.do_flush();
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
            return ready;
        }

        let ts = io_uring::types::Timespec::new()
            .sec(timeout.as_secs())
            .nsec(timeout.subsec_nanos() as _);
        let args = types::SubmitArgs::new().timespec(&ts);
        let _ = ring.submitter().submit_with_args(1, &args);
        self.pending_sqes.set(0);
        ring.completion().len()
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
        let pending_ops = unsafe { &mut *self.pending_ops.get() };
        let accept_results = unsafe { &mut *self.accept_results.get() };
        let slot_meta = unsafe { &mut *self.slot_meta.get() };
        let completed_ops = unsafe { &mut *self.completed_ops.get() };
        completions.clear();
        let reserve_hint = completed_ops.capacity().max(64);
        if completions.capacity() < reserve_hint {
            completions.reserve(reserve_hint - completions.capacity());
        }

        let mut cq = ring.completion();
        cq.sync();

        let mut processed = 0usize;
        for cqe in cq {
            let token_id = cqe.user_data();
            let result = cqe.result();
            let flags = cqe.flags();
            let has_more = cqueue::more(flags);

            if token_id == u64::MAX {
                continue;
            }

            let Some((idx, generation)) = Self::unpack_token(token_id) else {
                continue;
            };
            if idx >= slot_meta.len() || slot_meta[idx].generation != generation {
                continue;
            }

            let token = IoToken::from_id(slot_meta[idx].token_id);

            if has_more {
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
                        match socket_addr {
                            Ok(sa) => Ok(CompletionKind::Accept {
                                fd: result,
                                addr: Some(sa),
                            }),
                            Err(e) => Err(e),
                        }
                    } else {
                        Ok(CompletionKind::Accept {
                            fd: result,
                            addr: None,
                        })
                    };
                    res
                };
                let token_idx = token.slot_index();
                let queue = accept_results[token_idx].get_or_insert_with(VecDeque::new);
                queue.push_back(completion_result);
                // SAFETY: token belongs to this shard and the waker slot is
                // owned by the current io_state.
                crate::cpu::with_io_state(|io_state| unsafe {
                    if let Some(waker) = io_state.take_waker(token) {
                        waker.wake();
                    }
                });
                continue;
            }

            let pending_op = { pending_ops.try_remove(idx) };

            if let Some(pending_op) = pending_op {
                let completion_result = match pending_op {
                    PendingOp::Accept {
                        op: _op,
                        addr_storage,
                        addr_len,
                    } => {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else if let (Some(addr_storage), Some(addr_len)) =
                            (addr_storage.as_ref(), addr_len.as_ref())
                        {
                            let storage: *const libc::sockaddr_storage =
                                &**addr_storage as *const _;
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
                                        let ip = std::net::Ipv6Addr::from(
                                            sockaddr_in6.sin6_addr.s6_addr,
                                        );
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
                            match socket_addr {
                                Ok(sa) => Ok(CompletionKind::Accept {
                                    fd: result,
                                    addr: Some(sa),
                                }),
                                Err(e) => Err(e),
                            }
                        } else {
                            Ok(CompletionKind::Accept {
                                fd: result,
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
                    PendingOp::Write { op, data } => {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            match &op {
                                Op::WriteRent { .. } => Ok(CompletionKind::WriteRent {
                                    bytes_written: result as usize,
                                    data,
                                }),
                                _ => {
                                    let _ = data;
                                    Ok(CompletionKind::Write {
                                        bytes_written: result as usize,
                                    })
                                }
                            }
                        }
                    }
                    PendingOp::WriteStatic { op: _op } => {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::Write {
                                bytes_written: result as usize,
                            })
                        }
                    }
                    PendingOp::Fsync { op: _op } => {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::Fsync)
                        }
                    }
                    PendingOp::Close { op: _op } => {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::Close)
                        }
                    }
                    PendingOp::ReadFile { op: _op, mut buf } => {
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
                    PendingOp::WriteFile { op: _op, data } => {
                        let _ = data;
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::WriteFile {
                                bytes_written: result as usize,
                            })
                        }
                    }
                    PendingOp::UdpRecv {
                        op: _op,
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
                                let storage: *const libc::sockaddr_storage =
                                    &*addr_storage as *const _;
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
                        op: _op,
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
                    PendingOp::Connect {
                        op: _op,
                        addr_storage,
                    } => {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            let _ = &*addr_storage;
                            Ok(CompletionKind::Connect)
                        }
                    }
                    PendingOp::ReadVectored {
                        op: _op,
                        mut buffers,
                        iovecs,
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
                    PendingOp::WriteVectored {
                        op: _op,
                        buffers,
                        iovecs,
                    } => {
                        let _ = iovecs;
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::WriteVectored {
                                bytes_written: result as usize,
                                buffers,
                            })
                        }
                    }
                    PendingOp::PollAdd { op: _op } => {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::PollAdd {
                                events: result as u32,
                            })
                        }
                    }
                    PendingOp::Nop { op: _op } => {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::Nop)
                        }
                    }
                };
                completions.push((token, completion_result));
            }

            processed += 1;
            if processed >= self.drain_batch_size {
                break;
            }
        }

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

    fn supports_multishot_accept(&self) -> bool {
        true
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

    unsafe fn drain_completions_fused(
        &self,
        wakeup_poll_token: Option<IoToken>,
        batch_limit: usize,
    ) -> (usize, bool, usize) {
        self.drain_completions_fused_inline(wakeup_poll_token, batch_limit)
    }
}

#[cfg(test)]
mod stable_iovec_tests {
    use super::*;
    use smallvec::SmallVec;

    /// Structural proof for S0-4 / rm-06: `Box<[iovec]>` pointees stay fixed
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
                let iov = unsafe { *sqe_iov_base.add(i) };
                assert_eq!(iov.iov_len, 8);
                assert_eq!(unsafe { *(iov.iov_base as *const u8) }, i as u8);
            }
        }
    }
}
