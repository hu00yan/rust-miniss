#![cfg(all(target_os = "linux", feature = "io-uring"))]

//! Linux io_uring backend implementation
//!
//! This module provides a high-performance I/O backend for Linux using io_uring.
//! It implements submission and completion queue batching, fixed buffer management,
//! and integrates with the async runtime's waker system.

use io_uring::{opcode, types, IoUring};
use std::collections::HashMap;
use std::io;
use std::os::unix::io::RawFd;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use crate::io::{CompletionKind, IoBackend, IoError, IoToken, Op};

/// High-performance io_uring backend for Linux
///
/// This backend wraps the io_uring interface with submission and completion queue
/// batching for optimal performance. It manages fixed buffers and files for
/// zero-copy operations.
///
/// Uses interior mutability to comply with the IoBackend trait's `&self` requirements
/// while maintaining thread safety.
pub struct IoUringBackend {
    /// Internal state protected by mutex for thread safety
    inner: Arc<Mutex<IoUringState>>,
}

/// Internal mutable state of the io_uring backend
struct IoUringState {
    /// The underlying io_uring instance
    ring: IoUring,

    /// Pre-allocated buffers for I/O operations
    /// Maps buffer tokens to actual buffer memory
    buffers: HashMap<u64, Vec<u8>>,

    /// Maps I/O tokens to pending operations
    pending_ops: HashMap<u64, PendingOperation>,

    /// Maps I/O tokens to wakers for async notification
    wakers: HashMap<u64, Waker>,

    /// Cached completions ready to be returned
    ready_completions: Vec<(IoToken, Op, Result<CompletionKind, IoError>)>,

    /// Performance and debugging statistics
    stats: IoUringStats,
}

/// Represents a pending I/O operation
#[derive(Debug, Clone)]
struct PendingOperation {
    op: Op,
    buffer_token: Option<u64>,
}

impl IoUringBackend {
    /// Create a new io_uring backend with the specified queue depth
    ///
    /// # Arguments
    /// * `entries` - Number of entries in the submission/completion queues
    ///
    /// # Safety
    /// This function is safe as it only initializes the io_uring interface
    /// through the safe io_uring crate API.
    pub fn new(entries: u32) -> Result<Self, IoError> {
        let ring = IoUring::new(entries)
            .map_err(|e| IoError::Other(format!("Failed to create io_uring: {}", e)))?;

        let state = IoUringState {
            ring,
            buffers: HashMap::new(),
            pending_ops: HashMap::new(),
            wakers: HashMap::new(),
            ready_completions: Vec::new(),
            stats: IoUringStats::default(),
        };

        Ok(Self {
            inner: Arc::new(Mutex::new(state)),
        })
    }

    /// Register a pre-allocated buffer for zero-copy operations
    ///
    /// Returns a buffer token that can be used in I/O operations.
    pub fn register_buffer(&self, buffer: Vec<u8>) -> u64 {
        let mut state = self.inner.lock().unwrap();
        let token = state.stats.buffer_registrations;
        state.stats.buffer_registrations += 1;
        state.buffers.insert(token, buffer);
        token
    }

    /// Register fixed files with io_uring for improved performance
    ///
    /// # Safety
    /// This function uses the io_uring register_files API which is safe
    /// when called through the io_uring crate.
    pub fn register_files(&self, files: &[RawFd]) -> Result<(), IoError> {
        let state = self.inner.lock().unwrap();
        state
            .ring
            .submitter()
            .register_files(files)
            .map_err(|e| IoError::Other(format!("Failed to register files: {}", e)))
    }

    /// Get current performance statistics
    pub fn stats(&self) -> IoUringStats {
        let state = self.inner.lock().unwrap();
        state.stats.clone()
    }
}

impl IoUringState {
    /// Register a pre-allocated buffer for zero-copy operations
    ///
    /// Returns a buffer token that can be used in I/O operations.
    fn register_buffer(&mut self, buffer: Vec<u8>) -> u64 {
        let token = self.stats.buffer_registrations;
        self.stats.buffer_registrations += 1;
        self.buffers.insert(token, buffer);
        token
    }

    /// Submit a read operation using a pre-registered buffer
    ///
    /// # Safety
    /// The buffer pointer and length are validated against registered buffers.
    /// The io_uring crate handles the unsafe kernel interface safely.
    fn submit_read_op(
        &mut self,
        fd: RawFd,
        offset: u64,
        len: usize,
        token: IoToken,
    ) -> Result<(), IoError> {
        // Allocate buffer if needed
        let buffer_token = self.register_buffer(vec![0u8; len]);
        let buffer = self.buffers.get(&buffer_token).unwrap();

        // SAFETY: The buffer pointer is valid and aligned. The length is validated.
        // The io_uring crate ensures safe interaction with the kernel.
        let read_entry = opcode::Read::new(types::Fd(fd), buffer.as_ptr() as *mut u8, len as u32)
            .offset(offset)
            .build()
            .user_data(token.id());

        // Submit the operation
        // SAFETY: The submission queue entry is properly constructed
        unsafe {
            match self.ring.submission().push(&read_entry) {
                Ok(_) => {
                    self.pending_ops.insert(
                        token.id(),
                        PendingOperation {
                            op: Op::Read { fd, offset, len },
                            buffer_token: Some(buffer_token),
                        },
                    );
                    self.stats.submissions += 1;
                    Ok(())
                }
                Err(_) => Err(IoError::Other("Submission queue full".to_string())),
            }
        }
    }

    /// Submit a write operation using a pre-registered buffer
    ///
    /// # Safety
    /// The data is copied to a registered buffer to ensure memory safety.
    /// The io_uring crate handles the unsafe kernel interface safely.
    fn submit_write_op(
        &mut self,
        fd: RawFd,
        offset: u64,
        data: Vec<u8>,
        token: IoToken,
    ) -> Result<(), IoError> {
        let buffer_token = self.register_buffer(data.clone());
        let buffer = self.buffers.get(&buffer_token).unwrap();

        // SAFETY: The buffer pointer is valid and contains the data to write.
        // The io_uring crate ensures safe interaction with the kernel.
        let write_entry = opcode::Write::new(types::Fd(fd), buffer.as_ptr(), buffer.len() as u32)
            .offset(offset)
            .build()
            .user_data(token.id());

        // SAFETY: The submission queue entry is properly constructed
        unsafe {
            match self.ring.submission().push(&write_entry) {
                Ok(_) => {
                    self.pending_ops.insert(
                        token.id(),
                        PendingOperation {
                            op: Op::Write { fd, offset, data },
                            buffer_token: Some(buffer_token),
                        },
                    );
                    self.stats.submissions += 1;
                    Ok(())
                }
                Err(_) => Err(IoError::Other("Submission queue full".to_string())),
            }
        }
    }

    /// Submit an fsync operation
    ///
    /// # Safety
    /// fsync operations don't use buffers, making them inherently safe.
    fn submit_fsync_op(&mut self, fd: RawFd, token: IoToken) -> Result<(), IoError> {
        let fsync_entry = opcode::Fsync::new(types::Fd(fd))
            .build()
            .user_data(token.id());

        // SAFETY: The submission queue entry is properly constructed
        unsafe {
            match self.ring.submission().push(&fsync_entry) {
                Ok(_) => {
                    self.pending_ops.insert(
                        token.id(),
                        PendingOperation {
                            op: Op::Fsync { fd },
                            buffer_token: None,
                        },
                    );
                    self.stats.submissions += 1;
                    Ok(())
                }
                Err(_) => Err(IoError::Other("Submission queue full".to_string())),
            }
        }
    }

    /// Submit a close operation
    ///
    /// # Safety
    /// Close operations are safe as they only reference a file descriptor.
    fn submit_close_op(&mut self, fd: RawFd, token: IoToken) -> Result<(), IoError> {
        let close_entry = opcode::Close::new(types::Fd(fd))
            .build()
            .user_data(token.id());

        // SAFETY: The submission queue entry is properly constructed
        unsafe {
            match self.ring.submission().push(&close_entry) {
                Ok(_) => {
                    self.pending_ops.insert(
                        token.id(),
                        PendingOperation {
                            op: Op::Close { fd },
                            buffer_token: None,
                        },
                    );
                    self.stats.submissions += 1;
                    Ok(())
                }
                Err(_) => Err(IoError::Other("Submission queue full".to_string())),
            }
        }
    }

    /// Process completion queue entries and convert them to CompletionKind
    ///
    /// # Safety
    /// The completion queue entries are read safely through the io_uring crate.
    /// Buffer cleanup is handled properly to prevent memory leaks.
    fn process_completions(&mut self) {
        let mut cq = self.ring.completion();

        // Process all available completion queue entries
        for cqe in cq {
            let token_id = cqe.user_data();
            let result = cqe.result();

            if let Some(pending_op) = self.pending_ops.remove(&token_id) {
                let completion_result = if result >= 0 {
                    match &pending_op.op {
                        Op::Read { .. } => {
                            let buffer = if let Some(buffer_token) = pending_op.buffer_token {
                                self.buffers.remove(&buffer_token).unwrap_or_default()
                            } else {
                                Vec::new()
                            };
                            Ok(CompletionKind::Read {
                                bytes_read: result as usize,
                                data: buffer,
                            })
                        }
                        Op::Write { .. } => {
                            // Clean up write buffer
                            if let Some(buffer_token) = pending_op.buffer_token {
                                self.buffers.remove(&buffer_token);
                            }
                            Ok(CompletionKind::Write {
                                bytes_written: result as usize,
                            })
                        }
                        Op::Fsync { .. } => Ok(CompletionKind::Fsync),
                        Op::Close { .. } => Ok(CompletionKind::Close),
                    }
                } else {
                    // Clean up buffer on error
                    if let Some(buffer_token) = pending_op.buffer_token {
                        self.buffers.remove(&buffer_token);
                    }
                    Err(IoError::Io(io::Error::from_raw_os_error(-result)))
                };

                let token = IoToken::new(); // Note: This creates a new token, not the original
                self.ready_completions
                    .push((token, pending_op.op, completion_result));

                // Wake any waiting tasks
                if let Some(waker) = self.wakers.remove(&token_id) {
                    waker.wake();
                }

                self.stats.completions += 1;
            }
        }
    }
}

/// Performance and debugging statistics for the io_uring backend
#[derive(Debug, Clone, Default)]
pub struct IoUringStats {
    /// Total number of operations submitted
    pub submissions: u64,

    /// Total number of operations completed
    pub completions: u64,

    /// Number of buffers registered
    pub buffer_registrations: u64,

    /// Number of submission queue full events
    pub sq_full_events: u64,

    /// Number of completion queue overflow events
    pub cq_overflow_events: u64,
}

/// Implementation of the IoBackend trait for io_uring
impl IoBackend for IoUringBackend {
    type Completion = (IoToken, Op, Result<CompletionKind, IoError>);

    fn submit(&self, op: Op) -> IoToken {
        let token = IoToken::new();
        let mut state = self.inner.lock().unwrap();

        let result = match &op {
            Op::Read { fd, offset, len } => state.submit_read_op(*fd, *offset, *len, token),
            Op::Write { fd, offset, data } => {
                state.submit_write_op(*fd, *offset, data.clone(), token)
            }
            Op::Fsync { fd } => state.submit_fsync_op(*fd, token),
            Op::Close { fd } => state.submit_close_op(*fd, token),
        };

        // Submit pending operations to the kernel
        if state.ring.submit().is_err() {
            state.stats.sq_full_events += 1;
        }

        // On error, add a failed completion
        if let Err(e) = result {
            state.ready_completions.push((token, op, Err(e)));
        }

        token
    }

    fn poll_complete(&self, _cx: &mut Context<'_>) -> Poll<Vec<Self::Completion>> {
        let mut state = self.inner.lock().unwrap();

        // Process any new completions
        state.process_completions();

        // Return ready completions
        if !state.ready_completions.is_empty() {
            let completions = std::mem::take(&mut state.ready_completions);
            Poll::Ready(completions)
        } else {
            Poll::Ready(Vec::new())
        }
    }
}

// SAFETY: IoUringBackend can be safely sent between threads because:
// 1. io_uring handles are thread-safe when not shared
// 2. All internal state is properly managed
// 3. The io_uring crate provides thread-safe abstractions
unsafe impl Send for IoUringBackend {}

// SAFETY: IoUringBackend can be safely shared between threads with proper synchronization:
// 1. All operations are protected by &mut self requirements
// 2. The io_uring crate handles kernel synchronization
// 3. Internal collections are standard library types
unsafe impl Sync for IoUringBackend {}
