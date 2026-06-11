use crate::cpu::io_state;
use crate::io::IoToken;
use crate::Buffer;
use futures::io::{AsyncRead, AsyncWrite};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

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
    ///
    /// DNS resolution is performed synchronously (blocking), but the TCP
    /// handshake is fully async via `Op::Connect`.
    pub async fn connect<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        use std::net::ToSocketAddrs;

        let addr = addr.into();

        // Resolve DNS synchronously (blocking, but only for lookup).
        // For raw IPs this is instant; for hostnames it blocks briefly.
        let resolved = addr.to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "could not resolve address")
        })?;

        // Create a non-blocking TCP socket.
        let domain = if resolved.is_ipv4() {
            libc::AF_INET
        } else {
            libc::AF_INET6
        };
        let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // Set non-blocking via fcntl (SOCK_NONBLOCK is not available on macOS).
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        // Attempt non-blocking connect.
        let storage = socket_addr_to_storage(&resolved);
        let ret = unsafe {
            libc::connect(
                fd,
                &storage as *const libc::sockaddr_storage as *const libc::sockaddr,
                socket_addr_len(&resolved),
            )
        };

        if ret == 0 {
            // Connected immediately.
            let std_stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
            return Self::new(std_stream);
        }

        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINPROGRESS) {
            unsafe { libc::close(fd); }
            return Err(err);
        }

        // Submit Op::Connect to the I/O backend; wait for writable event.
        let std_stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
        let stream = Self::new(std_stream)?;

        let state = io_state();
        let op = crate::io::Op::Connect { fd, addr: resolved };
        let token = state.io_backend.submit(op);
        let future = crate::io::future::IoFuture::new(token);
        match future.await? {
            crate::io::CompletionKind::Connect => Ok(stream),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion",
            )),
        }
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
        let fd = self.inner.as_raw_fd();
        let state = crate::cpu::io_state();
        if let Some(token) = self.read_token {
            state.io_wakers.lock().unwrap().remove(&token);
            state.completed_io.lock().unwrap().remove(&token);
        }
        if let Some(token) = self.write_token {
            state.io_wakers.lock().unwrap().remove(&token);
            state.completed_io.lock().unwrap().remove(&token);
        }
        std::mem::forget(self);
        fd
    }
}

impl Drop for AsyncTcpStream {
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

impl AsyncRead for AsyncTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let state = io_state();
        let this = unsafe { self.as_mut().get_unchecked_mut() };
        if let Some(token) = this.read_token {
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
                state
                    .io_wakers
                    .lock()
                    .unwrap()
                    .insert(token, cx.waker().clone());
                Poll::Pending
            }
        } else {
            let op = crate::io::Op::Read {
                fd: this.inner.as_raw_fd(),
                offset: 0,
                len: buf.len(),
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
                state
                    .io_wakers
                    .lock()
                    .unwrap()
                    .insert(token, cx.waker().clone());
                Poll::Pending
            }
        } else {
            let op = crate::io::Op::Write {
                fd: this.inner.as_raw_fd(),
                offset: 0,
                data: Buffer::from_slice(buf),
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

fn socket_addr_to_storage(addr: &SocketAddr) -> libc::sockaddr_storage {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    match addr {
        SocketAddr::V4(a) => {
            let sin = libc::sockaddr_in {
                #[cfg(target_os = "macos")]
                sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
                sin_family: libc::AF_INET as _,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from(*a.ip()).to_be(),
                },
                sin_zero: [0; 8],
            };
            unsafe {
                std::ptr::write(
                    &mut storage as *mut _ as *mut libc::sockaddr_in,
                    sin,
                );
            }
        }
        SocketAddr::V6(a) => {
            let sin6 = libc::sockaddr_in6 {
                #[cfg(target_os = "macos")]
                sin6_len: std::mem::size_of::<libc::sockaddr_in6>() as u8,
                sin6_family: libc::AF_INET6 as _,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: a.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: a.ip().octets(),
                },
                sin6_scope_id: a.scope_id(),
            };
            unsafe {
                std::ptr::write(
                    &mut storage as *mut _ as *mut libc::sockaddr_in6,
                    sin6,
                );
            }
        }
    }
    storage
}

fn socket_addr_len(addr: &SocketAddr) -> libc::socklen_t {
    match addr {
        SocketAddr::V4(_) => std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        SocketAddr::V6(_) => std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
    }
}

impl AsRawFd for AsyncUdpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl IntoRawFd for AsyncUdpSocket {
    fn into_raw_fd(self) -> RawFd {
        let fd = self.inner.as_raw_fd();
        std::mem::forget(self);
        fd
    }
}
