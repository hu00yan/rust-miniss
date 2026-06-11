use crate::cpu::io_state;
use crate::io::IoToken;
use crate::Buffer;
use futures::io::{AsyncRead, AsyncWrite};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::pin::Pin;
use std::task::{Context, Poll};

// Rent traits for zero-copy (Glommio-style)
#[allow(async_fn_in_trait)]
pub trait AsyncReadRent {
    async fn poll_read_rent(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        len: usize,
    ) -> Poll<io::Result<Buffer>>;
}

#[allow(async_fn_in_trait)]
pub trait AsyncWriteRent {
    async fn poll_write_rent(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &Buffer,
    ) -> Poll<io::Result<usize>>;
}

/// An asynchronous TCP listener.
#[derive(Debug)]
pub struct AsyncTcpListener {
    inner: TcpListener,
}

impl AsyncTcpListener {
    /// Binds to the specified address to listen for incoming connections.
    pub fn bind<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        let listener = TcpListener::bind(addr.into())?;
        listener.set_nonblocking(true)?;
        Ok(Self { inner: listener })
    }

    /// Accepts a new incoming connection from this listener.
    pub async fn accept(&self) -> io::Result<(AsyncTcpStream, Option<SocketAddr>)> {
        let state = io_state();
        let op = crate::io::Op::Accept {
            fd: self.inner.as_raw_fd(),
        };
        let token = state.io_backend.submit(op);
        let future = crate::io::future::IoFuture::new(token);
        match future.await? {
            crate::io::CompletionKind::Accept { fd, addr } => {
                let stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
                let async_stream = AsyncTcpStream::new(stream)?;
                Ok((async_stream, addr))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion",
            )),
        }
    }

    /// Returns the local socket address of this listener.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

impl AsRawFd for AsyncTcpListener {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

/// An asynchronous TCP stream.
#[derive(Debug)]
pub struct AsyncTcpStream {
    inner: TcpStream,
    read_token: Option<IoToken>,
    write_token: Option<IoToken>,
}

impl AsyncTcpStream {
    /// Creates a new AsyncTcpStream from a TcpStream.
    pub fn new(stream: TcpStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            inner: stream,
            read_token: None,
            write_token: None,
        })
    }

    /// Connects to the specified address.
    pub async fn connect<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        let addr = addr.into();
        let std_stream = std::net::TcpStream::connect(addr)?;
        std_stream.set_nonblocking(true)?;
        Self::new(std_stream)
    }
}

impl From<TcpStream> for AsyncTcpStream {
    fn from(stream: TcpStream) -> Self {
        Self::new(stream).unwrap()
    }
}

impl AsRawFd for AsyncTcpStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl IntoRawFd for AsyncTcpStream {
    fn into_raw_fd(self) -> RawFd {
        // We need to forget self to avoid double cleanup
        let fd = self.inner.as_raw_fd();
        std::mem::forget(self);
        fd
    }
}

impl Drop for AsyncTcpStream {
    fn drop(&mut self) {
        // Clean up any in-progress operations
        let state = io_state();

        // Clean up read operation if in progress
        if let Some(token) = self.read_token {
            state.io_wakers.lock().unwrap().remove(&token);
            state.completed_io.lock().unwrap().remove(&token);
        }

        // Clean up write operation if in progress
        if let Some(token) = self.write_token {
            state.io_wakers.lock().unwrap().remove(&token);
            state.completed_io.lock().unwrap().remove(&token);
        }
    }
}

impl AsyncRead for AsyncTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let state = io_state();
        let this = unsafe { self.as_mut().get_unchecked_mut() };
        if let Some(token) = this.read_token {
            // Check if completed
            if let Some(result) = state.completed_io.lock().unwrap().remove(&token) {
                this.read_token = None;
                match result {
                    Ok(crate::io::CompletionKind::Read { bytes_read, data }) => {
                        let copy_len = std::cmp::min(bytes_read, buf.len());
                        buf[..copy_len].copy_from_slice(&data[..copy_len]);
                        Poll::Ready(Ok(copy_len))
                    }
                    Ok(_) => Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Unexpected completion",
                    ))),
                    Err(e) => Poll::Ready(Err(e.into())),
                }
            } else {
                // Still pending, register waker
                state
                    .io_wakers
                    .lock()
                    .unwrap()
                    .insert(token, cx.waker().clone());
                Poll::Pending
            }
        } else {
            // Start new read
            let op = crate::io::Op::Read {
                fd: this.inner.as_raw_fd(),
                offset: 0,
                len: buf.len(),
            };
            let token = state.io_backend.submit(op);
            this.read_token = Some(token);
            // Register waker and return pending
            state
                .io_wakers
                .lock()
                .unwrap()
                .insert(token, cx.waker().clone());
            Poll::Pending
        }
    }

    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [std::io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        let state = io_state();
        let this = unsafe { self.as_mut().get_unchecked_mut() };
        if let Some(token) = this.read_token {
            if let Some(result) = state.completed_io.lock().unwrap().remove(&token) {
                this.read_token = None;
                match result {
                    Ok(crate::io::CompletionKind::ReadVectored { bytes_read, buffers }) => {
                        let mut remaining = bytes_read;
                        for (owned_buf, caller_buf) in buffers.iter().zip(bufs.iter_mut()) {
                            let copy_len =
                                std::cmp::min(remaining, std::cmp::min(owned_buf.len(), caller_buf.len()));
                            caller_buf[..copy_len].copy_from_slice(&owned_buf[..copy_len]);
                            remaining -= copy_len;
                            if remaining == 0 {
                                break;
                            }
                        }
                        Poll::Ready(Ok(bytes_read))
                    }
                    Ok(_) => Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Unexpected completion",
                    ))),
                    Err(e) => Poll::Ready(Err(e.into())),
                }
            } else {
                state
                    .io_wakers
                    .lock()
                    .unwrap()
                    .insert(token, cx.waker().clone());
                Poll::Pending
            }
        } else {
            let buffers: Vec<crate::Buffer> = bufs
                .iter()
                .map(|b| crate::Buffer::with_capacity(b.len()))
                .collect();
            let op = crate::io::Op::ReadVectored {
                fd: this.inner.as_raw_fd(),
                buffers,
            };
            let token = state.io_backend.submit(op);
            this.read_token = Some(token);
            state
                .io_wakers
                .lock()
                .unwrap()
                .insert(token, cx.waker().clone());
            Poll::Pending
        }
    }
}

impl AsyncWrite for AsyncTcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let state = io_state();
        let this = unsafe { self.as_mut().get_unchecked_mut() };
        if let Some(token) = this.write_token {
            // Check if completed
            if let Some(result) = state.completed_io.lock().unwrap().remove(&token) {
                this.write_token = None;
                match result {
                    Ok(crate::io::CompletionKind::Write { bytes_written }) => {
                        Poll::Ready(Ok(bytes_written))
                    }
                    Ok(_) => Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Unexpected completion",
                    ))),
                    Err(e) => Poll::Ready(Err(e.into())),
                }
            } else {
                // Still pending, register waker
                state
                    .io_wakers
                    .lock()
                    .unwrap()
                    .insert(token, cx.waker().clone());
                Poll::Pending
            }
        } else {
            // Start new write
            let op = crate::io::Op::Write {
                fd: this.inner.as_raw_fd(),
                offset: 0,
                data: Buffer::from_slice(buf),
            };
            let token = state.io_backend.submit(op);
            this.write_token = Some(token);
            // Register waker and return pending
            state
                .io_wakers
                .lock()
                .unwrap()
                .insert(token, cx.waker().clone());
            Poll::Pending
        }
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let state = io_state();
        let this = unsafe { self.as_mut().get_unchecked_mut() };
        if let Some(token) = this.write_token {
            if let Some(result) = state.completed_io.lock().unwrap().remove(&token) {
                this.write_token = None;
                match result {
                    Ok(crate::io::CompletionKind::WriteVectored { bytes_written }) => {
                        Poll::Ready(Ok(bytes_written))
                    }
                    Ok(_) => Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Unexpected completion",
                    ))),
                    Err(e) => Poll::Ready(Err(e.into())),
                }
            } else {
                state
                    .io_wakers
                    .lock()
                    .unwrap()
                    .insert(token, cx.waker().clone());
                Poll::Pending
            }
        } else {
            let buffers: Vec<crate::Buffer> = bufs
                .iter()
                .map(|b| crate::Buffer::from_slice(b))
                .collect();
            let op = crate::io::Op::WriteVectored {
                fd: this.inner.as_raw_fd(),
                buffers,
            };
            let token = state.io_backend.submit(op);
            this.write_token = Some(token);
            state
                .io_wakers
                .lock()
                .unwrap()
                .insert(token, cx.waker().clone());
            Poll::Pending
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncReadRent for AsyncTcpStream {
    async fn poll_read_rent(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _len: usize,
    ) -> Poll<io::Result<Buffer>> {
        Poll::Ready(Err(io::Error::other("Not implemented")))
    }
}

impl AsyncWriteRent for AsyncTcpStream {
    async fn poll_write_rent(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &Buffer,
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::other("Not implemented")))
    }
}

// Placeholder implementations for other types
pub struct AsyncUnixStream {
    inner: UnixStream,
    read_token: Option<IoToken>,
    write_token: Option<IoToken>,
}

impl AsyncUnixStream {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            inner: stream,
            read_token: None,
            write_token: None,
        })
    }

    /// Connects to the specified Unix socket path.
    pub async fn connect<P: AsRef<std::path::Path>>(path: P) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Self::new(stream)
    }
}

impl AsRawFd for AsyncUnixStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl IntoRawFd for AsyncUnixStream {
    fn into_raw_fd(self) -> RawFd {
        // We need to forget self to avoid double cleanup
        let fd = self.inner.as_raw_fd();
        std::mem::forget(self);
        fd
    }
}

impl Drop for AsyncUnixStream {
    fn drop(&mut self) {
        let state = io_state();
        if let Some(token) = self.read_token {
            state.io_wakers.lock().unwrap().remove(&token);
            state.completed_io.lock().unwrap().remove(&token);
        }
        if let Some(token) = self.write_token {
            state.io_wakers.lock().unwrap().remove(&token);
            state.completed_io.lock().unwrap().remove(&token);
        }
    }
}

impl AsyncRead for AsyncUnixStream {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::other("Not implemented")))
    }

    fn poll_read_vectored(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _bufs: &mut [std::io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::other("Not implemented")))
    }
}

impl AsyncWrite for AsyncUnixStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::other("Not implemented")))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::other("Not implemented")))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

pub struct AsyncUnixListener {
    inner: UnixListener,
}

impl AsyncUnixListener {
    pub fn bind<P: AsRef<std::path::Path>>(path: P) -> io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        Ok(Self { inner: listener })
    }

    pub async fn accept(&self) -> io::Result<(AsyncUnixStream, std::os::unix::net::SocketAddr)> {
        Err(io::Error::other("Not implemented"))
    }
}

impl AsRawFd for AsyncUnixListener {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

pub struct AsyncUdpSocket {
    inner: UdpSocket,
}

impl AsyncUdpSocket {
    pub fn bind<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr.into())?;
        socket.set_nonblocking(true)?;
        Ok(Self { inner: socket })
    }

    /// Returns the local socket address of this socket.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let state = io_state();
        let op = crate::io::Op::UdpRecv {
            fd: self.inner.as_raw_fd(),
            buffer: Buffer::new_zeroed(buf.len()),
        };
        let token = state.io_backend.submit(op);
        let future = crate::io::future::IoFuture::new(token);
        match future.await? {
            crate::io::CompletionKind::UdpRecv {
                bytes_read,
                buffer,
                addr,
            } => {
                let copy_len = std::cmp::min(bytes_read, buf.len());
                buf[..copy_len].copy_from_slice(&buffer[..copy_len]);
                Ok((copy_len, addr))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion",
            )),
        }
    }

    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        let state = io_state();
        let op = crate::io::Op::UdpSend {
            fd: self.inner.as_raw_fd(),
            data: Buffer::from_slice(buf),
            addr: target,
        };
        let token = state.io_backend.submit(op);
        let future = crate::io::future::IoFuture::new(token);
        match future.await? {
            crate::io::CompletionKind::UdpSend { bytes_written, .. } => Ok(bytes_written),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion",
            )),
        }
    }
}

impl AsRawFd for AsyncUdpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl IntoRawFd for AsyncUdpSocket {
    fn into_raw_fd(self) -> RawFd {
        // We need to forget self to avoid double cleanup
        let fd = self.inner.as_raw_fd();
        std::mem::forget(self);
        fd
    }
}

// For UDP, users should use the native recv_from()/send_to() APIs.
