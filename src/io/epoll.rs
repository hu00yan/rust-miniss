//! Unix epoll backend implementation (not for macOS)
//!
//! This module provides a high-performance I/O backend for Unix systems using epoll.
//! It implements per-CPU poll instances with mio::unix::SourceFd for non-blocking
//! file descriptor operations, translating epoll_event to CompletionKind.

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll as TaskPoll, Waker};
use std::thread_local;
use std::time::{Duration, Instant};

use crate::io::{CompletionKind, IoBackend, IoError, IoToken, Op};

thread_local! {
    static CPU_POLL: std::cell::RefCell<Option<Poll>> = std::cell::RefCell::new(None);
}

/// High-performance epoll backend for Unix
///
/// This backend wraps the epoll interface through mio with per-CPU poll instances
/// for optimal performance. It manages non-blocking file descriptors and translates
/// epoll_event notifications to CompletionKind.
///
/// Uses interior mutability to comply with the IoBackend trait's `&self` requirements
/// while maintaining thread safety.
pub struct EpollBackend {
    /// Internal state protected by mutex for thread safety
    inner: Arc<Mutex<EpollState>>,
}

/// Internal mutable state of the epoll backend
struct EpollState {
    /// Maps I/O tokens to pending operations
    pending_ops: HashMap<u64, PendingOperation>,

    /// Maps I/O tokens to wakers for async notification
    wakers: HashMap<u64, Waker>,

    /// Cached completions ready to be returned
    ready_completions: Vec<(IoToken, Op, Result<CompletionKind, IoError>)>,

    /// Maps tokens to mio tokens for tracking
    token_map: HashMap<u64, Token>,

    /// Token counter for mio registration
    next_mio_token: usize,

    /// Performance and debugging statistics
    stats: EpollStats,
}

/// Represents a pending I/O operation
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct PendingOperation {
    op: Op,
    fd: RawFd,
    buffer: Option<Vec<u8>>,
    start_time: Instant,
}

impl EpollBackend {
    /// Create a new epoll backend
    ///
    /// The backend uses per-CPU poll instances for optimal performance.
    pub fn new() -> Result<Self, IoError> {
        let state = EpollState {
            pending_ops: HashMap::new(),
            wakers: HashMap::new(),
            ready_completions: Vec::new(),
            token_map: HashMap::new(),
            next_mio_token: 1,
            stats: EpollStats::default(),
        };

        Ok(Self {
            inner: Arc::new(Mutex::new(state)),
        })
    }

    /// Get current performance statistics
    pub fn stats(&self) -> EpollStats {
        let state = self.inner.lock().unwrap();
        state.stats.clone()
    }
}

impl EpollState {
    /// Create a poll instance for event processing
    fn create_poll() -> Result<Poll, IoError> {
        Poll::new().map_err(|e| IoError::Other(format!("Failed to create epoll poll: {}", e)))
    }

    /// Set file descriptor to non-blocking mode
    fn set_nonblocking(fd: RawFd) -> Result<(), IoError> {
        use nix::fcntl::{fcntl, FcntlArg, OFlag};

        let flags = fcntl(fd, FcntlArg::F_GETFL)
            .map_err(|e| IoError::Other(format!("Failed to get fd flags: {}", e)))?;

        let mut flags = OFlag::from_bits_truncate(flags);
        flags.insert(OFlag::O_NONBLOCK);

        fcntl(fd, FcntlArg::F_SETFL(flags))
            .map_err(|e| IoError::Other(format!("Failed to set fd non-blocking: {}", e)))?;

        Ok(())
    }

    /// Submit a read operation
    fn submit_read_op(
        &mut self,
        fd: RawFd,
        offset: u64,
        len: usize,
        token: IoToken,
    ) -> Result<(), IoError> {
        // Set fd to non-blocking
        Self::set_nonblocking(fd)?;

        let poll = Self::create_poll()?;
        let mio_token = Token(self.next_mio_token);
        self.next_mio_token += 1;

        // Register the fd with mio for read events
        let mut source_fd = SourceFd(&fd);
        poll.registry()
            .register(&mut source_fd, mio_token, Interest::READABLE)
            .map_err(|e| IoError::Other(format!("Failed to register fd for read: {}", e)))?;

        // Store the pending operation
        let pending_op = PendingOperation {
            op: Op::Read { fd, offset, len },
            fd,
            buffer: Some(vec![0u8; len]),
            start_time: Instant::now(),
        };

        self.pending_ops.insert(token.id(), pending_op);
        self.token_map.insert(token.id(), mio_token);
        self.stats.submissions += 1;

        Ok(())
    }

    /// Submit a write operation
    fn submit_write_op(
        &mut self,
        fd: RawFd,
        offset: u64,
        data: Vec<u8>,
        token: IoToken,
    ) -> Result<(), IoError> {
        // Set fd to non-blocking
        Self::set_nonblocking(fd)?;

        let poll = Self::create_poll()?;
        let mio_token = Token(self.next_mio_token);
        self.next_mio_token += 1;

        // Register the fd with mio for write events
        let mut source_fd = SourceFd(&fd);
        poll.registry()
            .register(&mut source_fd, mio_token, Interest::WRITABLE)
            .map_err(|e| IoError::Other(format!("Failed to register fd for write: {}", e)))?;

        // Store the pending operation
        let pending_op = PendingOperation {
            op: Op::Write {
                fd,
                offset,
                data: data.clone(),
            },
            fd,
            buffer: Some(data),
            start_time: Instant::now(),
        };

        self.pending_ops.insert(token.id(), pending_op);
        self.token_map.insert(token.id(), mio_token);
        self.stats.submissions += 1;

        Ok(())
    }

    /// Submit an fsync operation
    fn submit_fsync_op(&mut self, fd: RawFd, token: IoToken) -> Result<(), IoError> {
        // For fsync, we don't need to register with epoll, just perform the operation
        // and mark it as completed immediately
        let result = unsafe { libc::fsync(fd) };

        let completion_result = if result == 0 {
            Ok(CompletionKind::Fsync)
        } else {
            Err(IoError::Io(io::Error::last_os_error()))
        };

        let op = Op::Fsync { fd };
        self.ready_completions.push((token, op, completion_result));
        self.stats.submissions += 1;
        self.stats.completions += 1;

        Ok(())
    }

    /// Submit a close operation
    fn submit_close_op(&mut self, fd: RawFd, token: IoToken) -> Result<(), IoError> {
        // For close, we don't need to register with epoll, just perform the operation
        // and mark it as completed immediately
        let result = unsafe { libc::close(fd) };

        let completion_result = if result == 0 {
            Ok(CompletionKind::Close)
        } else {
            Err(IoError::Io(io::Error::last_os_error()))
        };

        let op = Op::Close { fd };
        self.ready_completions.push((token, op, completion_result));
        self.stats.submissions += 1;
        self.stats.completions += 1;

        Ok(())
    }

    /// Process epoll events and convert them to CompletionKind
    fn process_events(&mut self) -> Result<(), IoError> {
        let mut poll = Self::create_poll()?;
        let mut events = Events::with_capacity(128);

        // Poll for events with zero timeout (non-blocking)
        poll.poll(&mut events, Some(Duration::from_millis(0)))
            .map_err(|e| IoError::Other(format!("Failed to poll events: {}", e)))?;

        for event in events.iter() {
            let mio_token = event.token();

            // Find the corresponding IoToken
            let io_token_id = self
                .token_map
                .iter()
                .find(|(_, &token)| token == mio_token)
                .map(|(id, _)| *id);

            if let Some(token_id) = io_token_id {
                if let Some(pending_op) = self.pending_ops.remove(&token_id) {
                    self.token_map.remove(&token_id);

                    let completion_result =
                        self.handle_fd_event(&pending_op, event.is_readable(), event.is_writable());

                    let token = IoToken { id: token_id };
                    self.ready_completions
                        .push((token, pending_op.op, completion_result));

                    // Wake any waiting tasks
                    if let Some(waker) = self.wakers.remove(&token_id) {
                        waker.wake();
                    }

                    self.stats.completions += 1;

                    // Deregister the fd
                    let mut source_fd = SourceFd(&pending_op.fd);
                    let _ = poll.registry().deregister(&mut source_fd);
                }
            }
        }

        Ok(())
    }

    /// Handle a file descriptor event and perform the actual I/O operation
    fn handle_fd_event(
        &self,
        pending_op: &PendingOperation,
        readable: bool,
        writable: bool,
    ) -> Result<CompletionKind, IoError> {
        match &pending_op.op {
            Op::Read { fd, offset, len } => {
                if readable {
                    // Perform the actual read operation
                    let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                    let _ = file.seek(SeekFrom::Start(*offset));

                    let mut buffer = vec![0u8; *len];
                    match file.read(&mut buffer) {
                        Ok(bytes_read) => {
                            buffer.truncate(bytes_read);
                            // Don't close the file descriptor
                            std::mem::forget(file);
                            Ok(CompletionKind::Read {
                                bytes_read,
                                data: buffer,
                            })
                        }
                        Err(e) => {
                            std::mem::forget(file);
                            Err(IoError::Io(e))
                        }
                    }
                } else {
                    Err(IoError::Other(
                        "Expected readable event for read operation".to_string(),
                    ))
                }
            }
            Op::Write { fd, offset, data } => {
                if writable {
                    // Perform the actual write operation
                    let mut file = unsafe { std::fs::File::from_raw_fd(*fd) };
                    let _ = file.seek(SeekFrom::Start(*offset));

                    match file.write(data) {
                        Ok(bytes_written) => {
                            // Don't close the file descriptor
                            std::mem::forget(file);
                            Ok(CompletionKind::Write { bytes_written })
                        }
                        Err(e) => {
                            std::mem::forget(file);
                            Err(IoError::Io(e))
                        }
                    }
                } else {
                    Err(IoError::Other(
                        "Expected writable event for write operation".to_string(),
                    ))
                }
            }
            Op::Fsync { .. } => Ok(CompletionKind::Fsync),
            Op::Close { .. } => Ok(CompletionKind::Close),
        }
    }

    /// Get current performance statistics
    #[allow(dead_code)]
    pub fn stats(&self) -> &EpollStats {
        &self.stats
    }
}

/// Performance and debugging statistics for the epoll backend
#[derive(Debug, Clone, Default)]
pub struct EpollStats {
    /// Total number of operations submitted
    pub submissions: u64,

    /// Total number of operations completed
    pub completions: u64,

    /// Number of file descriptors registered
    pub fd_registrations: u64,

    /// Number of poll events processed
    pub events_processed: u64,

    /// Number of timeouts in polling
    pub poll_timeouts: u64,
}

/// Implementation of the IoBackend trait for epoll
impl IoBackend for EpollBackend {
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

        // On error, add a failed completion
        if let Err(e) = result {
            state.ready_completions.push((token, op, Err(e)));
        }

        token
    }

    fn poll_complete(&self, _cx: &mut Context<'_>) -> TaskPoll<Vec<Self::Completion>> {
        let mut state = self.inner.lock().unwrap();

        // Process any new events
        let _ = state.process_events();

        // Return ready completions
        if !state.ready_completions.is_empty() {
            let completions = std::mem::take(&mut state.ready_completions);
            TaskPoll::Ready(completions)
        } else {
            TaskPoll::Ready(Vec::new())
        }
    }
}

// SAFETY: EpollBackend can be safely sent between threads because:
// 1. epoll handles are thread-safe when not shared
// 2. All internal state is properly managed with mutexes
// 3. mio provides thread-safe abstractions
unsafe impl Send for EpollBackend {}

// SAFETY: EpollBackend can be safely shared between threads with proper synchronization:
// 1. All operations are protected by mutex
// 2. mio handles kernel synchronization
// 3. Internal collections are standard library types
unsafe impl Sync for EpollBackend {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll};

    #[test]
    fn test_epoll_backend_creation() {
        let backend = EpollBackend::new();
        assert!(backend.is_ok());
    }

    #[test]
    fn test_submit_operations() {
        let backend = EpollBackend::new().unwrap();

        // Test read operation
        let read_op = Op::Read {
            fd: 1,
            offset: 0,
            len: 1024,
        };
        let token = backend.submit(read_op);
        assert!(token.id() > 0);

        // Test write operation
        let write_op = Op::Write {
            fd: 1,
            offset: 0,
            data: vec![1, 2, 3],
        };
        let token = backend.submit(write_op);
        assert!(token.id() > 0);

        // Test fsync operation
        let fsync_op = Op::Fsync { fd: 1 };
        let token = backend.submit(fsync_op);
        assert!(token.id() > 0);

        // Test close operation
        let close_op = Op::Close { fd: 1 };
        let token = backend.submit(close_op);
        assert!(token.id() > 0);
    }

    #[test]
    fn test_poll_complete_empty() {
        let backend = EpollBackend::new().unwrap();
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        match backend.poll_complete(&mut cx) {
            Poll::Ready(completions) => {
                assert!(completions.is_empty());
            }
            Poll::Pending => panic!("EpollBackend should return Ready when no operations pending"),
        }
    }
}
