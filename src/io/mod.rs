pub trait IoBackend: Send + Sync + 'static {
    type Completion;

    fn submit(&self, op: Op) -> IoToken;

    fn poll_complete(&self, cx: &mut Context<'_>) -> Poll<Vec<Self::Completion>>;
}

// Define types needed for the trait
#[derive(Debug, Clone)]
pub enum Op {
    Read { fd: i32, offset: u64, len: usize },
    Write { fd: i32, offset: u64, data: Vec<u8> },
    Fsync { fd: i32 },
    Close { fd: i32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IoToken {
    id: u64,
}

static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(1);

impl Default for IoToken {
    fn default() -> Self {
        Self::new()
    }
}

impl IoToken {
    pub fn new() -> Self {
        Self {
            id: TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }
}

#[derive(Debug, Clone)]
pub enum CompletionKind {
    Read { bytes_read: usize, data: Vec<u8> },
    Write { bytes_written: usize },
    Fsync,
    Close,
}

#[derive(Debug)]
pub enum IoError {
    Io(std::io::Error),
    Other(String),
}

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IoError::Io(err) => write!(f, "IO error: {err}"),
            IoError::Other(msg) => write!(f, "Other error: {msg}"),
        }
    }
}

impl std::error::Error for IoError {}

use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

// Conditional compilation for different I/O backends
#[cfg(any(target_os = "macos", feature = "kqueue"))]
pub mod kqueue;

#[cfg(any(all(unix, not(target_os = "macos")), feature = "epoll"))]
pub mod epoll;

#[cfg(all(target_os = "linux", feature = "io-uring"))]
pub mod uring;

#[cfg(any(target_os = "macos", feature = "kqueue"))]
pub use kqueue::KqueueBackend;

#[cfg(any(all(unix, not(target_os = "macos")), feature = "epoll"))]
pub use epoll::EpollBackend;

#[cfg(all(target_os = "linux", feature = "io-uring"))]
pub use uring::IoUringBackend;

// Dummy backend for testing - provides no-op implementations
pub struct DummyIoBackend {
    simulate_completions: bool,
}

impl Default for DummyIoBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl DummyIoBackend {
    pub fn new() -> Self {
        Self {
            simulate_completions: false,
        }
    }

    pub fn with_completions() -> Self {
        Self {
            simulate_completions: true,
        }
    }

    pub fn simulate_completion(op: &Op) -> Result<CompletionKind, IoError> {
        match op {
            Op::Read { len, .. } => Ok(CompletionKind::Read {
                bytes_read: *len,
                data: vec![0u8; *len],
            }),
            Op::Write { data, .. } => Ok(CompletionKind::Write {
                bytes_written: data.len(),
            }),
            Op::Fsync { .. } => Ok(CompletionKind::Fsync),
            Op::Close { .. } => Ok(CompletionKind::Close),
        }
    }
}

impl IoBackend for DummyIoBackend {
    type Completion = (IoToken, Op, Result<CompletionKind, IoError>);

    fn submit(&self, _op: Op) -> IoToken {
        IoToken::new()
    }

    fn poll_complete(&self, _cx: &mut Context<'_>) -> Poll<Vec<Self::Completion>> {
        if self.simulate_completions {
            // In a real implementation, this would check for completed operations
            // For testing, we just return empty to avoid infinite completions
            Poll::Ready(vec![])
        } else {
            Poll::Ready(vec![])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll};

    #[test]
    fn test_io_token_uniqueness() {
        let token1 = IoToken::new();
        let token2 = IoToken::new();
        assert_ne!(token1.id(), token2.id());
    }

    #[test]
    fn test_dummy_backend_submit() {
        let backend = DummyIoBackend::new();
        let op = Op::Read {
            fd: 1,
            offset: 0,
            len: 1024,
        };
        let token = backend.submit(op);
        assert!(token.id() > 0);
    }

    #[test]
    fn test_dummy_backend_poll_complete() {
        let backend = DummyIoBackend::new();
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        match backend.poll_complete(&mut cx) {
            Poll::Ready(completions) => {
                assert!(completions.is_empty());
            }
            Poll::Pending => panic!("DummyIoBackend should always return Ready"),
        }
    }

    #[test]
    fn test_simulate_completion_read() {
        let op = Op::Read {
            fd: 1,
            offset: 0,
            len: 100,
        };
        let result = DummyIoBackend::simulate_completion(&op);

        match result {
            Ok(CompletionKind::Read { bytes_read, data }) => {
                assert_eq!(bytes_read, 100);
                assert_eq!(data.len(), 100);
                assert!(data.iter().all(|&b| b == 0));
            }
            _ => panic!("Expected Read completion"),
        }
    }

    #[test]
    fn test_simulate_completion_write() {
        let data = vec![1, 2, 3, 4, 5];
        let op = Op::Write {
            fd: 1,
            offset: 0,
            data: data.clone(),
        };
        let result = DummyIoBackend::simulate_completion(&op);

        match result {
            Ok(CompletionKind::Write { bytes_written }) => {
                assert_eq!(bytes_written, data.len());
            }
            _ => panic!("Expected Write completion"),
        }
    }

    #[test]
    fn test_io_error_display() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err = IoError::Io(io_err);
        let display = format!("{}", err);
        assert!(display.contains("IO error"));

        let other_err = IoError::Other("custom error".to_string());
        let display = format!("{}", other_err);
        assert_eq!(display, "Other error: custom error");
    }
}
