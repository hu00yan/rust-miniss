#![cfg(all(target_os = "linux", io_backend = "io_uring"))]

//! A `io-uring` backend for the I/O subsystem.

use crate::io::{CompletionKind, IoBackend, IoError, IoToken, Op};
use io_uring::{opcode, types, IoUring};
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::io;
use std::task::{Context, Poll};

/// A struct to hold a pending operation and any associated data, like buffers.
enum PendingOp {
    Read { op: Op, buf: Vec<u8> },
    Write { op: Op },
    Accept { op: Op },
    Fsync { op: Op },
    Close { op: Op },
    ReadFile { op: Op, buf: Vec<u8> },
    WriteFile { op: Op, data: Vec<u8> },
}

/// A `io-uring` based `IoBackend`.
pub struct UringBackend {
    ring: UnsafeCell<IoUring>,
    pending_ops: UnsafeCell<HashMap<u64, PendingOp>>,
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
        })
    }
}

impl IoBackend for UringBackend {
    type Completion = (IoToken, Op, Result<CompletionKind, IoError>);

    fn submit(&self, op: Op) -> IoToken {
        let token = IoToken::new();
        let user_data = token.id();

        // SAFETY: We have exclusive access on this thread.
        let ring = unsafe { &mut *self.ring.get() };
        let pending_ops = unsafe { &mut *self.pending_ops.get() };

        let (entry, pending_op) = match op {
            Op::Accept { fd } => {
                let entry = opcode::Accept::new(types::Fd(fd), std::ptr::null_mut(), std::ptr::null_mut())
                    .build()
                    .user_data(user_data);
                (entry, PendingOp::Accept{ op: Op::Accept { fd } })
            }
            Op::Read { fd, offset, len } => {
                let mut buf = vec![0u8; len];
                let entry = opcode::Read::new(types::Fd(fd), buf.as_mut_ptr(), len as u32)
                    .offset(offset)
                    .build()
                    .user_data(user_data);
                (entry, PendingOp::Read { op: Op::Read { fd, offset, len }, buf })
            }
            Op::Write { fd, offset, ref data } => {
                let entry = opcode::Write::new(types::Fd(fd), data.as_ptr(), data.len() as u32)
                    .offset(offset)
                    .build()
                    .user_data(user_data);
                // We clone the op here to own the data.
                (entry, PendingOp::Write { op: Op::Write { fd, offset, data: data.clone() } })
            }
            Op::Fsync { fd } => {
                let entry = opcode::Fsync::new(types::Fd(fd))
                    .build()
                    .user_data(user_data);
                (entry, PendingOp::Fsync { op: Op::Fsync { fd } })
            }
            Op::Close { fd } => {
                let entry = opcode::Close::new(types::Fd(fd))
                    .build()
                    .user_data(user_data);
                (entry, PendingOp::Close { op: Op::Close { fd } })
            }
            Op::ReadFile { fd, offset, len } => {
                let mut buf = vec![0u8; len];
                let entry = opcode::Read::new(types::Fd(fd), buf.as_mut_ptr(), len as u32)
                    .offset(offset)
                    .build()
                    .user_data(user_data);
                (entry, PendingOp::ReadFile { op: Op::ReadFile { fd, offset, len }, buf })
            }
            Op::WriteFile { fd, offset, ref data } => {
                // For io_uring WriteFile operations, we must ensure the data buffer
                // remains valid throughout the entire I/O operation. This is critical
                // because io_uring uses the buffer pointer directly (zero-copy).
                //
                // The issue we fixed: Previously, we used `data.as_ptr()` where `data`
                // was borrowed via `ref data`. When the function returned, this borrow
                // could become invalid, leading to writing garbage data to the file.
                //
                // Solution: Clone the data first to ensure it stays alive, then use
                // the cloned data's pointer. The cloned data is stored in PendingOp
                // to keep it alive until the operation completes.
                let cloned_data = data.clone();
                let entry = opcode::Write::new(types::Fd(fd), cloned_data.as_ptr(), cloned_data.len() as u32)
                    .offset(offset)
                    .build()
                    .user_data(user_data);
                // Store both the original op (for completion reporting) and the cloned
                // data (to keep it alive until completion)
                (entry, PendingOp::WriteFile { op: Op::WriteFile { fd, offset, data: data.clone() }, data: cloned_data })
            }
        };

        match unsafe { ring.submission().push(&entry) } {
            Ok(_) => {
                pending_ops.insert(user_data, pending_op);
            }
            Err(e) => {
                eprintln!("Failed to submit io-uring operation: {}", e);
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
                let token = IoToken { id: token_id };

                let (op, completion_result) = match pending_op {
                    PendingOp::Accept { op } => (op, {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::Accept { fd: result, addr: None })
                        }
                    }),
                    PendingOp::Read { op, mut buf } => (op, {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            let bytes_read = result as usize;
                            buf.truncate(bytes_read);
                            Ok(CompletionKind::Read { bytes_read, data: buf })
                        }
                    }),
                    PendingOp::Write { op } => (op, {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::Write { bytes_written: result as usize })
                        }
                    }),
                    PendingOp::Fsync { op } => (op, {
                        if result < 0 { Err(IoError::Io(io::Error::from_raw_os_error(-result))) } else { Ok(CompletionKind::Fsync) }
                    }),
                    PendingOp::Close { op } => (op, {
                        if result < 0 { Err(IoError::Io(io::Error::from_raw_os_error(-result))) } else { Ok(CompletionKind::Close) }
                    }),
                    PendingOp::ReadFile { op, mut buf } => (op, {
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            let bytes_read = result as usize;
                            buf.truncate(bytes_read);
                            Ok(CompletionKind::ReadFile { bytes_read, data: buf })
                        }
                    }),
                    PendingOp::WriteFile { op, data: _data } => (op, {
                        println!("Processing completed WriteFile operation, result={}", result);
                        if result < 0 {
                            Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                        } else {
                            Ok(CompletionKind::WriteFile { bytes_written: result as usize })
                        }
                    }),
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
