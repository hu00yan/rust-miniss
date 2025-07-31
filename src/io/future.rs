use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::io::Result as IoResult;
use std::sync::{Arc, Weak};

use crate::io::backend::IoBackend;
use crate::io::token::IoToken;

/// Future representing an asynchronous I/O operation
pub struct IoFuture {
    /// Token identifying this I/O operation
    token: IoToken,
    /// Weak reference to the backend to avoid circular references
    backend: Weak<dyn IoBackend>,
    /// Stored waker for when the operation completes
    waker: Option<Waker>,
    /// Whether this future has been completed
    completed: bool,
}

impl IoFuture {
    /// Create a new IoFuture
    pub(crate) fn new(token: IoToken, backend: Weak<dyn IoBackend>) -> Self {
        Self {
            token,
            backend,
            waker: None,
            completed: false,
        }
    }

    /// Get the token for this future
    pub fn token(&self) -> IoToken {
        self.token
    }
}

impl Future for IoFuture {
    type Output = IoResult<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // If already completed, we shouldn't be polled again
        if self.completed {
            return Poll::Pending;
        }

        // Try to get a strong reference to the backend
        let backend = match self.backend.upgrade() {
            Some(backend) => backend,
            None => {
                // Backend has been dropped, operation is cancelled
                self.completed = true;
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "I/O backend was dropped"
                )));
            }
        };

        // Check if the operation is complete
        if let Some(result) = backend.check_completion(self.token) {
            self.completed = true;
            return Poll::Ready(result);
        }

        // Operation not ready yet, store the waker
        self.waker = Some(cx.waker().clone());
        backend.register_waker(self.token, cx.waker().clone());

        Poll::Pending
    }
}

impl Drop for IoFuture {
    fn drop(&mut self) {
        // If the operation hasn't completed yet, try to cancel it
        if !self.completed {
            if let Some(backend) = self.backend.upgrade() {
                backend.cancel_operation(self.token);
            }
        }
    }
}

/// Helper functions for creating common I/O operations
impl IoFuture {
    /// Read data from a file descriptor at a specific offset
    pub async fn read_at(
        backend: Arc<dyn IoBackend>,
        fd: i32,
        buffer: &mut [u8],
        offset: u64,
    ) -> IoResult<usize> {
        let token = backend.submit_read(fd, buffer.as_mut_ptr(), buffer.len(), offset)?;
        let future = IoFuture::new(token, Arc::downgrade(&backend));
        future.await
    }

    /// Write data to a file descriptor at a specific offset
    pub async fn write_at(
        backend: Arc<dyn IoBackend>,
        fd: i32,
        buffer: &[u8],
        offset: u64,
    ) -> IoResult<usize> {
        let token = backend.submit_write(fd, buffer.as_ptr(), buffer.len(), offset)?;
        let future = IoFuture::new(token, Arc::downgrade(&backend));
        future.await
    }

    /// Synchronize a file descriptor to storage
    pub async fn fsync(backend: Arc<dyn IoBackend>, fd: i32) -> IoResult<usize> {
        let token = backend.submit_fsync(fd)?;
        let future = IoFuture::new(token, Arc::downgrade(&backend));
        future.await
    }

    /// Synchronize a file descriptor's data to storage (without metadata)
    pub async fn fdatasync(backend: Arc<dyn IoBackend>, fd: i32) -> IoResult<usize> {
        let token = backend.submit_fdatasync(fd)?;
        let future = IoFuture::new(token, Arc::downgrade(&backend));
        future.await
    }

    /// Open a file asynchronously
    pub async fn open(
        backend: Arc<dyn IoBackend>,
        path: &str,
        flags: i32,
        mode: u32,
    ) -> IoResult<usize> {
        let token = backend.submit_open(path, flags, mode)?;
        let future = IoFuture::new(token, Arc::downgrade(&backend));
        future.await
    }

    /// Close a file descriptor asynchronously
    pub async fn close(backend: Arc<dyn IoBackend>, fd: i32) -> IoResult<usize> {
        let token = backend.submit_close(fd)?;
        let future = IoFuture::new(token, Arc::downgrade(&backend));
        future.await
    }

    /// Read data from a file descriptor (without offset)
    pub async fn read(
        backend: Arc<dyn IoBackend>,
        fd: i32,
        buffer: &mut [u8],
    ) -> IoResult<usize> {
        Self::read_at(backend, fd, buffer, 0).await
    }

    /// Write data to a file descriptor (without offset)
    pub async fn write(
        backend: Arc<dyn IoBackend>,
        fd: i32,
        buffer: &[u8],
    ) -> IoResult<usize> {
        Self::write_at(backend, fd, buffer, 0).await
    }

    /// Poll for readiness on a file descriptor
    pub async fn poll_read(backend: Arc<dyn IoBackend>, fd: i32) -> IoResult<usize> {
        let token = backend.submit_poll_read(fd)?;
        let future = IoFuture::new(token, Arc::downgrade(&backend));
        future.await
    }

    /// Poll for write readiness on a file descriptor
    pub async fn poll_write(backend: Arc<dyn IoBackend>, fd: i32) -> IoResult<usize> {
        let token = backend.submit_poll_write(fd)?;
        let future = IoFuture::new(token, Arc::downgrade(&backend));
        future.await
    }

    /// Accept a connection on a listening socket
    pub async fn accept(backend: Arc<dyn IoBackend>, fd: i32) -> IoResult<usize> {
        let token = backend.submit_accept(fd)?;
        let future = IoFuture::new(token, Arc::downgrade(&backend));
        future.await
    }

    /// Connect to a remote address
    pub async fn connect(
        backend: Arc<dyn IoBackend>,
        fd: i32,
        addr: &std::net::SocketAddr,
    ) -> IoResult<usize> {
        let token = backend.submit_connect(fd, addr)?;
        let future = IoFuture::new(token, Arc::downgrade(&backend));
        future.await
    }

    /// Send data on a socket

    /// Test IoFuture for successful completion
    #[tokio::test]
    async fn test_iofuture_successful_completion() {
        let backend = Arc::new(MockBackend::new());
        let mut buffer = vec![0u8; 512];

        let future = IoFuture::read_at(backend.clone(), 1, [0;31m[0;31m&mut[0m buffer, 0);

        // Simulate completion
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        let token = IoToken::new(1); // First token
        backend.complete_operation(token, Ok(128));

        let result = future.await;
        assert_eq!(result.unwrap(), 128);
    }

    /// Test IoFuture for cancellation race
    #[tokio::test]
    async fn test_iofuture_cancellation_race() {
        let backend = Arc::new(MockBackend::new());
        let mut buffer = vec![0u8; 128];

        let future = IoFuture::read_at(backend.clone(), 1, [0;31m[0;31m&mut[0m buffer, 0);
        drop(future); // Should trigger cancellation

        // Confirm cancellation
        let completions = backend.completions.lock().unwrap();
        assert!(completions.contains_key(&IoToken::new(1)));
    }    

    pub async fn sync
        backend: Arc<dyn IoBackend>,
        fd: i32,
        buffer: &[u8],
        flags: i32,
    ) -> IoResult<usize> {
        let token = backend.submit_send(fd, buffer.as_ptr(), buffer.len(), flags)?;
        let future = IoFuture::new(token, Arc::downgrade(&backend));
        future.await
    }

    /// Receive data from a socket
    pub async fn recv(
        backend: Arc<dyn IoBackend>,
        fd: i32,
        buffer: &mut [u8],
        flags: i32,
    ) -> IoResult<usize> {
        let token = backend.submit_recv(fd, buffer.as_mut_ptr(), buffer.len(), flags)?;
        let future = IoFuture::new(token, Arc::downgrade(&backend));
        future.await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::task::Waker;

    // Mock backend for testing
    struct MockBackend {
        next_token: AtomicU64,
        completions: Mutex<HashMap<IoToken, IoResult<usize>>>,
        wakers: Mutex<HashMap<IoToken, Waker>>,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                next_token: AtomicU64::new(1),
                completions: Mutex::new(HashMap::new()),
                wakers: Mutex::new(HashMap::new()),
            }
        }

        fn complete_operation(&self, token: IoToken, result: IoResult<usize>) {
            {
                let mut completions = self.completions.lock().unwrap();
                completions.insert(token, result);
            }
            
            // Wake up any waiting future
            let mut wakers = self.wakers.lock().unwrap();
            if let Some(waker) = wakers.remove(&token) {
                waker.wake();
            }
        }
    }

    impl IoBackend for MockBackend {
        fn submit_read(&self, _fd: i32, _buf: *mut u8, _len: usize, _offset: u64) -> IoResult<IoToken> {
            let token = IoToken::new(self.next_token.fetch_add(1, Ordering::SeqCst));
            Ok(token)
        }

        fn submit_write(&self, _fd: i32, _buf: *const u8, _len: usize, _offset: u64) -> IoResult<IoToken> {
            let token = IoToken::new(self.next_token.fetch_add(1, Ordering::SeqCst));
            Ok(token)
        }

        fn submit_fsync(&self, _fd: i32) -> IoResult<IoToken> {
            let token = IoToken::new(self.next_token.fetch_add(1, Ordering::SeqCst));
            Ok(token)
        }

        fn submit_fdatasync(&self, _fd: i32) -> IoResult<IoToken> {
            let token = IoToken::new(self.next_token.fetch_add(1, Ordering::SeqCst));
            Ok(token)
        }

        fn submit_open(&self, _path: &str, _flags: i32, _mode: u32) -> IoResult<IoToken> {
            let token = IoToken::new(self.next_token.fetch_add(1, Ordering::SeqCst));
            Ok(token)
        }

        fn submit_close(&self, _fd: i32) -> IoResult<IoToken> {
            let token = IoToken::new(self.next_token.fetch_add(1, Ordering::SeqCst));
            Ok(token)
        }

        fn submit_poll_read(&self, _fd: i32) -> IoResult<IoToken> {
            let token = IoToken::new(self.next_token.fetch_add(1, Ordering::SeqCst));
            Ok(token)
        }

        fn submit_poll_write(&self, _fd: i32) -> IoResult<IoToken> {
            let token = IoToken::new(self.next_token.fetch_add(1, Ordering::SeqCst));
            Ok(token)
        }

        fn submit_accept(&self, _fd: i32) -> IoResult<IoToken> {
            let token = IoToken::new(self.next_token.fetch_add(1, Ordering::SeqCst));
            Ok(token)
        }

        fn submit_connect(&self, _fd: i32, _addr: &std::net::SocketAddr) -> IoResult<IoToken> {
            let token = IoToken::new(self.next_token.fetch_add(1, Ordering::SeqCst));
            Ok(token)
        }

        fn submit_send(&self, _fd: i32, _buf: *const u8, _len: usize, _flags: i32) -> IoResult<IoToken> {
            let token = IoToken::new(self.next_token.fetch_add(1, Ordering::SeqCst));
            Ok(token)
        }

        fn submit_recv(&self, _fd: i32, _buf: *mut u8, _len: usize, _flags: i32) -> IoResult<IoToken> {
            let token = IoToken::new(self.next_token.fetch_add(1, Ordering::SeqCst));
            Ok(token)
        }

        fn check_completion(&self, token: IoToken) -> Option<IoResult<usize>> {
            let completions = self.completions.lock().unwrap();
            completions.get(&token).cloned()
        }

        fn register_waker(&self, token: IoToken, waker: Waker) {
            let mut wakers = self.wakers.lock().unwrap();
            wakers.insert(token, waker);
        }

        fn cancel_operation(&self, token: IoToken) {
            let mut completions = self.completions.lock().unwrap();
            completions.insert(token, Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Operation cancelled"
            )));
        }
    }

    #[tokio::test]
    async fn test_future_completion() {
        let backend = Arc::new(MockBackend::new());
        let mut buffer = vec![0u8; 1024];
        
        let future_handle = {
            let backend_clone = backend.clone();
            tokio::spawn(async move {
                IoFuture::read_at(backend_clone, 1, &mut buffer, 0).await
            })
        };

        // Simulate completion after a delay
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        let token = IoToken::new(1); // First token
        backend.complete_operation(token, Ok(512));

        let result = future_handle.await.unwrap();
        assert_eq!(result.unwrap(), 512);
    }

    #[tokio::test]
    async fn test_future_cancellation() {
        let backend = Arc::new(MockBackend::new());
        let mut buffer = vec![0u8; 1024];
        
        let future = IoFuture::read_at(backend.clone(), 1, &mut buffer, 0);
        drop(future); // This should trigger cancellation

        // The operation should be cancelled in the backend
        let token = IoToken::new(1);
        let completions = backend.completions.lock().unwrap();
        assert!(completions.contains_key(&token));
    }
}
