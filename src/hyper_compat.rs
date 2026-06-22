//! Hyper integration for rust-miniss.
//!
//! Implements `hyper::rt::{Read, Write, Executor, Timer}` traits so that
//! hyper can run on the rust-miniss runtime without Tokio.
//!
//! # Example
//!
//! ```rust,no_run
//! use rust_miniss::net::AsyncTcpListener;
//! use rust_miniss::hyper_compat::{HyperExecutor, TcpIo};
//! use hyper::service::service_fn;
//! use hyper::{Request, Response};
//! use hyper::body::Incoming;
//! use http_body_util::Full;
//! use bytes::Bytes;
//!
//! async fn handler(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
//!     Ok(Response::new(Full::new(Bytes::from("Hello from rust-miniss!"))))
//! }
//!
//! # async fn example() {
//! let executor = HyperExecutor::new();
//! let addr: std::net::SocketAddr = "0.0.0.0:8080".parse().unwrap();
//! let listener = AsyncTcpListener::bind(addr).unwrap();
//! loop {
//!     let (stream, _) = listener.accept().await.unwrap();
//!     executor.spawn(async move {
//!         let io = TcpIo::from_async(stream);
//!         hyper::server::conn::http1::Builder::new()
//!             .serve_connection(io, service_fn(handler))
//!             .await
//!             .unwrap();
//!     });
//! }
//! # }
//! ```

use crate::net::AsyncTcpStream;
use crate::timer;
use hyper::rt::ReadBufCursor;
use std::future::Future;
use std::io;
use std::net::TcpStream as StdTcpStream;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// Wrapper that implements `hyper::rt::{Read, Write}` for a TCP stream.
///
/// This uses standard non-blocking I/O, allowing it to work on any thread
/// (including Tokio runtime threads).
pub struct TcpIo {
    stream: StdTcpStream,
}

impl TcpIo {
    /// Create a new TcpIo from a standard TcpStream.
    ///
    /// The stream must be in non-blocking mode.
    pub fn new(stream: StdTcpStream) -> Self {
        stream.set_nonblocking(true).unwrap();
        Self { stream }
    }

    /// Create a new TcpIo from an AsyncTcpStream.
    ///
    /// This extracts the underlying file descriptor and creates a new
    /// standard TcpStream that can be used on any thread.
    pub fn from_async(async_stream: AsyncTcpStream) -> Self {
        let fd = async_stream.as_raw_fd();
        // Duplicate the fd so we own it independently
        let new_fd = unsafe { libc::dup(fd) };
        if new_fd < 0 {
            panic!("Failed to duplicate file descriptor");
        }
        // Forget the async stream to prevent it from closing the fd
        std::mem::forget(async_stream);

        let stream = unsafe { StdTcpStream::from_raw_fd(new_fd) };
        Self::new(stream)
    }
}

impl io::Read for TcpIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut self.stream, buf)
    }
}

impl io::Write for TcpIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut self.stream, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.stream)
    }
}

impl hyper::rt::Read for TcpIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let slice = unsafe { buf.as_mut() };
        let read_buf =
            unsafe { std::slice::from_raw_parts_mut(slice.as_mut_ptr().cast(), slice.len()) };
        match io::Read::read(&mut self.stream, read_buf) {
            Ok(n) => {
                unsafe {
                    buf.advance(n);
                }
                Poll::Ready(Ok(()))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl hyper::rt::Write for TcpIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        match io::Write::write(&mut self.stream, buf) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(io::Write::flush(&mut self.get_mut().stream))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(self.get_mut().stream.shutdown(std::net::Shutdown::Write))
    }
}

impl AsRawFd for TcpIo {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}

impl tokio::io::AsyncRead for TcpIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let slice = buf.initialize_unfilled();
        match io::Read::read(&mut self.stream, slice) {
            Ok(n) => {
                buf.advance(n);
                Poll::Ready(Ok(()))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl tokio::io::AsyncWrite for TcpIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        match io::Write::write(&mut self.stream, buf) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(io::Write::flush(&mut self.get_mut().stream))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(self.get_mut().stream.shutdown(std::net::Shutdown::Write))
    }
}

/// Executor that spawns tasks using a dedicated Tokio runtime.
///
/// This executor maintains a background Tokio runtime for hyper's internal needs.
/// The runtime is created lazily on first use and leaked to avoid drop issues.
#[derive(Clone)]
pub struct HyperExecutor;

static HYPER_RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

fn hyper_runtime() -> &'static tokio::runtime::Runtime {
    HYPER_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("Failed to create Tokio runtime for hyper")
    })
}

impl HyperExecutor {
    /// Create a new HyperExecutor.
    pub fn new() -> Self {
        Self
    }

    /// Spawn a future onto the Tokio runtime.
    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        hyper_runtime().spawn(future);
    }

    /// Spawn a future and return a JoinHandle.
    pub fn spawn_with_handle<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        hyper_runtime().spawn(future)
    }
}

impl Default for HyperExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl<F> hyper::rt::Executor<F> for HyperExecutor
where
    F: Future<Output = ()> + Send + 'static,
{
    fn execute(&self, future: F) {
        hyper_runtime().spawn(future);
    }
}

/// Timer implementation using rust-miniss's timer wheel.
pub struct MinissTimer;

impl hyper::rt::Timer for MinissTimer {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn hyper::rt::Sleep>> {
        Box::pin(MinissSleep {
            future: Box::pin(timer::sleep(duration)),
        })
    }

    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn hyper::rt::Sleep>> {
        let now = Instant::now();
        let duration = if deadline > now {
            deadline - now
        } else {
            Duration::ZERO
        };
        Box::pin(MinissSleep {
            future: Box::pin(timer::sleep(duration)),
        })
    }
}

struct MinissSleep {
    future: Pin<Box<dyn Future<Output = ()> + Send + Sync>>,
}

impl std::future::Future for MinissSleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.future.as_mut().poll(cx)
    }
}

unsafe impl Sync for MinissSleep {}

impl hyper::rt::Sleep for MinissSleep {}

/// Convenience function to create a hyper-compatible IO wrapper for an AsyncTcpStream.
pub fn wrap_tcp(stream: AsyncTcpStream) -> TcpIo {
    TcpIo::from_async(stream)
}

/// Extension trait to add hyper compatibility to AsyncTcpStream.
pub trait HyperTcpExt {
    fn hyper_io(self) -> TcpIo
    where
        Self: Sized;
}

impl HyperTcpExt for AsyncTcpStream {
    fn hyper_io(self) -> TcpIo {
        TcpIo::from_async(self)
    }
}
