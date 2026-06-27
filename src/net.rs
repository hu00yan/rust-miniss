use crate::cpu::{current_task_id, io_state};
use crate::io::IoToken;
use crate::Buffer;
use futures::io::{AsyncRead, AsyncWrite};
use libc;
use socket2::{Domain, Protocol, Socket, Type};
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
    /// Bind with SO_REUSEPORT so one listener can be created per worker core.
    pub fn bind_reuse_port<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        let addr = addr.into();
        let domain = if addr.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };
        let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        socket.set_nonblocking(true)?;
        socket.bind(&addr.into())?;
        socket.listen(1024)?;
        let listener: TcpListener = socket.into();
        listener.set_nonblocking(true)?;
        Ok(Self { inner: listener })
    }

    pub async fn accept(&self) -> io::Result<(AsyncTcpStream, Option<SocketAddr>)> {
        let state = io_state();
        let op = crate::io::Op::Accept {
            fd: self.inner.as_raw_fd(),
        };
        let token = state.io_backend.submit(op);
        let future = crate::io::future::IoFuture::new(token);
        let result = future.await;
        match result? {
            crate::io::CompletionKind::Accept { fd, addr } => {
                let stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
                let async_stream = AsyncTcpStream::new(stream)?;
                Ok((async_stream, addr))
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unexpected completion: {other:?}"),
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
    /// Token for POLLOUT submitted after a Write returned EAGAIN.
    /// The task is woken when the socket becomes writable, then
    /// a fresh Write is submitted.
    pollout_token: Option<IoToken>,
}

impl AsyncTcpStream {
    /// Creates a new AsyncTcpStream from a TcpStream.
    pub fn new(stream: TcpStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        stream.set_nodelay(true)?;
        // Increase send/recv buffer to reduce EAGAIN frequency under load.
        // Default 16KB is too small for high-throughput echo benchmarks.
        let fd = stream.as_raw_fd();
        unsafe {
            let size: u32 = 256 * 1024;
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &size as *const _ as *const libc::c_void,
                std::mem::size_of::<u32>() as u32,
            );
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &size as *const _ as *const libc::c_void,
                std::mem::size_of::<u32>() as u32,
            );
        }
        Ok(Self {
            inner: stream,
            read_token: None,
            write_token: None,
            pollout_token: None,
        })
    }

    /// Read into a user-provided buffer (owned-buffer pattern).
    /// Returns the buffer back with data filled in.
    /// This avoids the internal copy of the standard AsyncRead path.
    pub async fn read_rent(&self, buffer: Buffer) -> io::Result<(usize, Buffer)> {
        let state = io_state();
        let op = crate::io::Op::ReadRent {
            fd: self.inner.as_raw_fd(),
            buffer,
        };
        let token = state.io_backend.submit(op);
        let future = crate::io::future::IoFuture::new(token);
        match future.await? {
            crate::io::CompletionKind::ReadRent { buffer } => {
                let n = buffer.len();
                Ok((n, buffer))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion",
            )),
        }
    }

    /// Write from a user-provided buffer (owned-buffer pattern).
    /// Returns the buffer back so it can be reused for the next read/write.
    pub async fn write_rent(&self, data: Buffer) -> io::Result<(usize, Buffer)> {
        let state = io_state();
        let op = crate::io::Op::WriteRent {
            fd: self.inner.as_raw_fd(),
            data,
        };
        let token = state.io_backend.submit(op);
        let future = crate::io::future::IoFuture::new(token);
        match future.await? {
            crate::io::CompletionKind::WriteRent {
                bytes_written,
                data,
            } => Ok((bytes_written, data)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion",
            )),
        }
    }

    /// Write multiple owned buffers with one vectored I/O operation.
    /// Returns the buffers so callers can recycle or reuse them.
    pub async fn write_vectored_rent(
        &self,
        buffers: Vec<Buffer>,
    ) -> io::Result<(usize, Vec<Buffer>)> {
        let state = io_state();
        let op = crate::io::Op::WriteVectored {
            fd: self.inner.as_raw_fd(),
            buffers,
        };
        let token = state.io_backend.submit(op);
        let future = crate::io::future::IoFuture::new(token);
        match future.await? {
            crate::io::CompletionKind::WriteVectored {
                bytes_written,
                buffers,
            } => Ok((bytes_written, buffers)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion",
            )),
        }
    }

    /// Write a static byte slice without copying it into an owned Buffer.
    pub async fn write_static_all(&self, mut data: &'static [u8]) -> io::Result<()> {
        while !data.is_empty() {
            let state = io_state();
            let op = crate::io::Op::WriteStatic {
                fd: self.inner.as_raw_fd(),
                data,
            };
            let token = state.io_backend.submit(op);
            let future = crate::io::future::IoFuture::new(token);
            match future.await {
                Ok(crate::io::CompletionKind::Write { bytes_written }) => {
                    if bytes_written == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "write_static_all wrote zero bytes",
                        ));
                    }
                    data = &data[bytes_written..];
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Unexpected completion",
                    ));
                }
                Err(crate::io::IoError::Io(e))
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.raw_os_error() == Some(libc::EAGAIN) =>
                {
                    self.wait_writable().await?;
                }
                Err(e) => return Err(e.into()),
            }
        }

        Ok(())
    }

    async fn wait_writable(&self) -> io::Result<()> {
        let state = io_state();
        let op = crate::io::Op::PollAdd {
            fd: self.inner.as_raw_fd(),
            events: libc::POLLOUT as u32,
            multishot: false,
        };
        let token = state.io_backend.submit(op);
        let future = crate::io::future::IoFuture::new(token);
        match future.await? {
            crate::io::CompletionKind::PollAdd { .. } => Ok(()),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion",
            )),
        }
    }

    /// Connects to the specified address.
    pub async fn connect<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        use std::net::ToSocketAddrs;

        let addr = addr.into();
        let resolved = addr.to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "could not resolve address")
        })?;

        let domain = if resolved.is_ipv4() {
            libc::AF_INET
        } else {
            libc::AF_INET6
        };
        let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        let storage = socket_addr_to_storage(&resolved);
        let ret = unsafe {
            libc::connect(
                fd,
                &storage as *const libc::sockaddr_storage as *const libc::sockaddr,
                socket_addr_len(&resolved),
            )
        };

        if ret == 0 {
            let std_stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
            return Self::new(std_stream);
        }

        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINPROGRESS) {
            unsafe {
                libc::close(fd);
            }
            return Err(err);
        }

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
        // SAFETY: Single-threaded access.
        unsafe {
            if let Some(token) = self.read_token {
                state.take_waker(token);
                state.take_completed(token);
                state.take_pending_task(token);
            }
            if let Some(token) = self.write_token {
                state.take_waker(token);
                state.take_completed(token);
                state.take_pending_task(token);
            }
            if let Some(token) = self.pollout_token {
                state.take_waker(token);
                state.take_completed(token);
                state.take_pending_task(token);
            }
        }
        std::mem::forget(self);
        fd
    }
}

impl Drop for AsyncTcpStream {
    fn drop(&mut self) {
        if let Some(state) = crate::cpu::try_io_state() {
            // SAFETY: Single-threaded access.
            unsafe {
                if let Some(token) = self.read_token {
                    state.take_waker(token);
                    state.take_completed(token);
                    state.take_pending_task(token);
                }
                if let Some(token) = self.write_token {
                    state.take_waker(token);
                    state.take_completed(token);
                    state.take_pending_task(token);
                }
                if let Some(token) = self.pollout_token {
                    state.take_waker(token);
                    state.take_completed(token);
                    state.take_pending_task(token);
                }
            }
        }
    }
}

impl AsyncRead for AsyncTcpStream {
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let state = io_state();
        let this = unsafe { self.as_mut().get_unchecked_mut() };
        let token = if let Some(token) = this.read_token {
            // SAFETY: Single-threaded access.
            if let Some(result) = unsafe { state.take_completed(token) } {
                this.read_token = None;
                return match result {
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
                };
            }
            token
        } else {
            let op = crate::io::Op::Read {
                fd: this.inner.as_raw_fd(),
                offset: 0,
                len: buf.len(),
            };
            let token = state.io_backend.submit(op);
            this.read_token = Some(token);
            token
        };
        // Register waker AND pending_task for inline completion.
        // SAFETY: Single-threaded access.
        unsafe {
            state.store_waker(token, cx.waker().clone());
            if let Some(task_id) = current_task_id() {
                state.store_pending_task(token, task_id);
            }
        }
        Poll::Pending
    }

    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [std::io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        let state = io_state();
        let this = unsafe { self.as_mut().get_unchecked_mut() };
        let token = if let Some(token) = this.read_token {
            // SAFETY: Single-threaded access.
            if let Some(result) = unsafe { state.take_completed(token) } {
                this.read_token = None;
                return match result {
                    Ok(crate::io::CompletionKind::ReadVectored {
                        bytes_read,
                        buffers,
                    }) => {
                        let mut remaining = bytes_read;
                        for (owned_buf, caller_buf) in buffers.iter().zip(bufs.iter_mut()) {
                            let copy_len = std::cmp::min(
                                remaining,
                                std::cmp::min(owned_buf.len(), caller_buf.len()),
                            );
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
                };
            }
            token
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
            token
        };
        // SAFETY: Single-threaded access.
        unsafe {
            state.store_waker(token, cx.waker().clone());
            if let Some(task_id) = current_task_id() {
                state.store_pending_task(token, task_id);
            }
        }
        Poll::Pending
    }
}

impl AsyncWrite for AsyncTcpStream {
    #[inline]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let state = io_state();
        let this = unsafe { self.as_mut().get_unchecked_mut() };

        // Clear stale POLLOUT completion (fire-and-forget).
        // POLLOUT just serves as a wakeup mechanism when the socket
        // becomes writable after an EAGAIN; the completion itself is
        // discarded, then poll_write submits a fresh Write SQE.
        if let Some(pollout_tok) = this.pollout_token.take() {
            unsafe {
                state.take_waker(pollout_tok);
                state.take_completed(pollout_tok);
                state.take_pending_task(pollout_tok);
            }
        }

        let token = if let Some(token) = this.write_token {
            // SAFETY: Single-threaded access.
            if let Some(result) = unsafe { state.take_completed(token) } {
                this.write_token = None;
                return match result {
                    Ok(crate::io::CompletionKind::Write { bytes_written }) => {
                        Poll::Ready(Ok(bytes_written))
                    }
                    Ok(_) => Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Unexpected completion",
                    ))),
                    Err(ref e) if is_write_wouldblock(e) => {
                        // Socket send buffer full. Submit POLLOUT to be
                        // notified when buffer space becomes available.
                        // Then return Pending; the task will be woken by
                        // POLLOUT and retry the write with a fresh SQE.
                        let pollout_op = crate::io::Op::PollAdd {
                            fd: this.inner.as_raw_fd(),
                            events: libc::POLLOUT as u32,
                            multishot: false,
                        };
                        let pollout_tok = state.io_backend.submit(pollout_op);
                        this.pollout_token = Some(pollout_tok);
                        unsafe {
                            state.store_waker(pollout_tok, cx.waker().clone());
                            if let Some(task_id) = current_task_id() {
                                state.store_pending_task(pollout_tok, task_id);
                            }
                        }
                        return Poll::Pending;
                    }
                    Err(e) => Poll::Ready(Err(e.into())),
                };
            }
            token
        } else {
            let op = crate::io::Op::Write {
                fd: this.inner.as_raw_fd(),
                offset: 0,
                data: Buffer::from_slice(buf),
            };
            let token = state.io_backend.submit(op);
            this.write_token = Some(token);
            token
        };
        // SAFETY: Single-threaded access.
        unsafe {
            state.store_waker(token, cx.waker().clone());
            if let Some(task_id) = current_task_id() {
                state.store_pending_task(token, task_id);
            }
        }
        Poll::Pending
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let state = io_state();
        let this = unsafe { self.as_mut().get_unchecked_mut() };

        // Clear stale POLLOUT completion (fire-and-forget).
        if let Some(pollout_tok) = this.pollout_token.take() {
            unsafe {
                state.take_waker(pollout_tok);
                state.take_completed(pollout_tok);
                state.take_pending_task(pollout_tok);
            }
        }

        let token = if let Some(token) = this.write_token {
            // SAFETY: Single-threaded access.
            if let Some(result) = unsafe { state.take_completed(token) } {
                this.write_token = None;
                return match result {
                    Ok(crate::io::CompletionKind::WriteVectored { bytes_written, .. }) => {
                        Poll::Ready(Ok(bytes_written))
                    }
                    Ok(_) => Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Unexpected completion",
                    ))),
                    Err(ref e) if is_write_wouldblock(e) => {
                        // Same as poll_write: EAGAIN → POLLOUT → retry
                        let pollout_op = crate::io::Op::PollAdd {
                            fd: this.inner.as_raw_fd(),
                            events: libc::POLLOUT as u32,
                            multishot: false,
                        };
                        let pollout_tok = state.io_backend.submit(pollout_op);
                        this.pollout_token = Some(pollout_tok);
                        unsafe {
                            state.store_waker(pollout_tok, cx.waker().clone());
                            if let Some(task_id) = current_task_id() {
                                state.store_pending_task(pollout_tok, task_id);
                            }
                        }
                        return Poll::Pending;
                    }
                    Err(e) => Poll::Ready(Err(e.into())),
                };
            }
            token
        } else {
            let buffers: Vec<crate::Buffer> =
                bufs.iter().map(|b| crate::Buffer::from_slice(b)).collect();
            let op = crate::io::Op::WriteVectored {
                fd: this.inner.as_raw_fd(),
                buffers,
            };
            let token = state.io_backend.submit(op);
            this.write_token = Some(token);
            token
        };
        // SAFETY: Single-threaded access.
        unsafe {
            state.store_waker(token, cx.waker().clone());
            if let Some(task_id) = current_task_id() {
                state.store_pending_task(token, task_id);
            }
        }
        Poll::Pending
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
        let fd = socket.as_raw_fd();
        unsafe {
            let size: u32 = 4 * 1024 * 1024;
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &size as *const _ as *const libc::c_void,
                std::mem::size_of::<u32>() as u32,
            );
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &size as *const _ as *const libc::c_void,
                std::mem::size_of::<u32>() as u32,
            );
        }
        Ok(Self { inner: socket })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let state = io_state();
        let op = crate::io::Op::UdpRecv {
            fd: self.inner.as_raw_fd(),
            buffer: Buffer::new_uninitialized(buf.len()),
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
                std::ptr::write(&mut storage as *mut _ as *mut libc::sockaddr_in, sin);
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
                std::ptr::write(&mut storage as *mut _ as *mut libc::sockaddr_in6, sin6);
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

/// Check if an IoError represents a "write would block" condition (EAGAIN/EWOULDBLOCK).
/// io_uring returns -EAGAIN when the socket send buffer is full.
fn is_write_wouldblock(e: &crate::io::IoError) -> bool {
    match e {
        crate::io::IoError::Io(io_err) => {
            io_err.raw_os_error() == Some(libc::EAGAIN)
                || io_err.raw_os_error() == Some(libc::EWOULDBLOCK)
        }
        _ => false,
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
