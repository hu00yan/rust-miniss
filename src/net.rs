use crate::cpu::{current_task_id, try_with_io_state, with_io_state};
use crate::io::IoToken;
use crate::{Buffer, RentBuffer};
use futures::io::{AsyncRead, AsyncWrite};
use hyper::rt::{Read as HyperRead, ReadBufCursor, Write as HyperWrite};
use libc;
use smallvec::SmallVec;
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::io::IoSlice;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

const TCP_BUFFER_SIZE: usize = 256 * 1024;

/// An asynchronous TCP listener.
#[derive(Debug)]
pub struct AsyncTcpListener {
    inner: TcpListener,
    accept_token: Option<IoToken>,
}

impl AsyncTcpListener {
    /// Binds to the specified address to listen for incoming connections.
    pub fn bind<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        let listener = bind_tcp_listener(addr.into(), false)?;
        Ok(Self {
            inner: listener,
            accept_token: None,
        })
    }

    /// Accepts a new incoming connection from this listener.
    /// Bind with SO_REUSEPORT so one listener can be created per worker core.
    pub fn bind_reuse_port<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        let listener = bind_tcp_listener(addr.into(), true)?;
        Ok(Self {
            inner: listener,
            accept_token: None,
        })
    }

    pub async fn accept(&mut self) -> io::Result<(AsyncTcpStream, Option<SocketAddr>)> {
        if self.accept_token.is_none() {
            match self.inner.accept() {
                Ok((stream, addr)) => {
                    let async_stream = AsyncTcpStream::new(stream)?;
                    return Ok((async_stream, Some(addr)));
                }
                Err(err) if is_wouldblock(&err) => {}
                Err(err) => return Err(err),
            }
        }
        let token = match self.accept_token {
            Some(token) => token,
            None => {
                let op = crate::io::Op::Accept {
                    fd: self.inner.as_raw_fd(),
                };
                let token =
                    with_io_state(|state| state.io_backend.submit(op)).map_err(io::Error::from)?;
                self.accept_token = Some(token);
                token
            }
        };
        let future = crate::io::future::IoFuture::accept(token);
        let result = future.await;
        let (supports_multishot, streams_nonblocking) = with_io_state(|state| {
            (
                state.io_backend.supports_multishot_accept(),
                state.io_backend.accepted_streams_are_nonblocking(),
            )
        });
        if !supports_multishot {
            self.accept_token = None;
        }
        // Terminal CQE/error disarms multishot operation.  Do not retain a
        // dead token and wait forever on the next accept call; next call will
        // issue a fresh operation (or use the synchronous fallback).
        if result.is_err() {
            self.accept_token = None;
        }
        match result? {
            crate::io::CompletionKind::Accept { fd, addr } => {
                // `CompletionKind::Accept` owns the descriptor until this
                // conversion; TcpStream takes ownership exactly once.
                let stream = std::net::TcpStream::from(fd);
                let async_stream = if streams_nonblocking {
                    AsyncTcpStream::from_nonblocking(stream)?
                } else {
                    AsyncTcpStream::new(stream)?
                };
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

impl Drop for AsyncTcpListener {
    fn drop(&mut self) {
        let Some(token) = self.accept_token.take() else {
            return;
        };
        let _ = try_with_io_state(|state| {
            if state.io_backend.cancel(token) {
                state.io_backend.flush();
            }
        });
    }
}

fn bind_tcp_listener(addr: SocketAddr, reuse_port: bool) -> io::Result<TcpListener> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    if reuse_port {
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
    }
    socket.set_send_buffer_size(TCP_BUFFER_SIZE)?;
    socket.set_recv_buffer_size(TCP_BUFFER_SIZE)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
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
        Self::from_nonblocking(stream)
    }

    /// Creates a new AsyncTcpStream from a TcpStream that is already non-blocking.
    fn from_nonblocking(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self {
            inner: stream,
            read_token: None,
            write_token: None,
            pollout_token: None,
        })
    }

    /// Read into a user-provided owned buffer.
    ///
    /// The buffer is converted into the runtime's internal owned-buffer form,
    /// filled by the kernel, then returned to the caller's type without an
    /// extra copy.
    pub async fn read_rent<T>(&self, buffer: T) -> io::Result<(usize, T)>
    where
        T: RentBuffer,
    {
        let fd = self.inner.as_raw_fd();
        let mut buffer = buffer.into();
        // SAFETY: The owned buffer provides a writable region up to its full
        // capacity; the slice is only used for this synchronous syscall.
        match try_read(fd, unsafe {
            std::slice::from_raw_parts_mut(buffer.as_mut_ptr(), buffer.capacity())
        }) {
            Ok(bytes_read) => {
                // SAFETY: `try_read` initialized exactly `bytes_read` bytes.
                unsafe {
                    buffer.set_len(bytes_read);
                }
                Ok((bytes_read, T::from(buffer)))
            }
            Err(err) if is_wouldblock(&err) => {
                let op = crate::io::Op::ReadRent { fd, buffer };
                let token = with_io_state(|state| state.io_backend.submit(op));
                let future = crate::io::future::IoFuture::new(token);
                match future.await? {
                    crate::io::CompletionKind::ReadRent { buffer } => {
                        let n = buffer.len();
                        Ok((n, T::from(buffer)))
                    }
                    other => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Unexpected completion for read_rent: {other:?}"),
                    )),
                }
            }
            Err(err) => Err(err),
        }
    }

    /// Write from a user-provided owned buffer.
    ///
    /// The runtime consumes the caller's buffer, hands it to the kernel, and
    /// returns the same allocation after completion.
    pub async fn write_rent<T>(&self, data: T) -> io::Result<(usize, T)>
    where
        T: RentBuffer,
    {
        let fd = self.inner.as_raw_fd();
        let data = data.into();
        match try_write(fd, &data) {
            Ok(bytes_written) => {
                if bytes_written == 0 && !data.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write_rent wrote zero bytes",
                    ));
                }
                Ok((bytes_written, T::from(data)))
            }
            Err(err) if is_wouldblock(&err) => {
                let op = crate::io::Op::WriteRent { fd, data };
                let token = with_io_state(|state| state.io_backend.submit(op));
                let future = crate::io::future::IoFuture::new(token);
                match future.await? {
                    crate::io::CompletionKind::WriteRent {
                        bytes_written,
                        data,
                    } => Ok((bytes_written, T::from(data))),
                    _ => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Unexpected completion",
                    )),
                }
            }
            Err(err) => Err(err),
        }
    }

    /// Write multiple owned buffers in one call.
    /// Returns the buffers so callers can recycle or reuse them.
    pub async fn write_buffers(&self, buffers: Vec<Buffer>) -> io::Result<(usize, Vec<Buffer>)> {
        let op = crate::io::Op::WriteVectored {
            fd: self.inner.as_raw_fd(),
            buffers: buffers.into(),
        };
        let token = with_io_state(|state| state.io_backend.submit(op));
        let future = crate::io::future::IoFuture::new(token);
        match future.await? {
            crate::io::CompletionKind::WriteVectored {
                bytes_written,
                buffers,
            } => Ok((bytes_written, buffers.into_vec())),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion",
            )),
        }
    }

    /// Write two byte slices fully.
    pub async fn write_all_slices(&mut self, first: &[u8], second: &[u8]) -> io::Result<()> {
        let mut first = first;
        let mut second = second;

        while !first.is_empty() || !second.is_empty() {
            let n = if second.is_empty() {
                futures::future::poll_fn(|cx| {
                    AsyncWrite::poll_write(Pin::new(&mut *self), cx, first)
                })
                .await?
            } else if first.is_empty() {
                futures::future::poll_fn(|cx| {
                    AsyncWrite::poll_write(Pin::new(&mut *self), cx, second)
                })
                .await?
            } else {
                let bufs = [IoSlice::new(first), IoSlice::new(second)];
                futures::future::poll_fn(|cx| {
                    AsyncWrite::poll_write_vectored(Pin::new(&mut *self), cx, &bufs)
                })
                .await?
            };
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "write_all_slices wrote zero bytes",
                ));
            }

            let first_len = first.len();
            if n < first_len {
                first = &first[n..];
                continue;
            }

            first = &[];
            let remaining = n - first_len;
            if remaining >= second.len() {
                second = &[];
            } else {
                second = &second[remaining..];
            }
        }

        Ok(())
    }

    /// Write a static byte slice without copying it into an owned Buffer.
    pub async fn write_static_all(&self, mut data: &'static [u8]) -> io::Result<()> {
        while !data.is_empty() {
            let op = crate::io::Op::WriteStatic {
                fd: self.inner.as_raw_fd(),
                data,
            };
            let token = with_io_state(|state| state.io_backend.submit(op));
            let future = crate::io::future::IoFuture::new(token);
            match future.await? {
                crate::io::CompletionKind::Write { bytes_written } => {
                    if bytes_written == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "write_static_all wrote zero bytes",
                        ));
                    }
                    data = &data[bytes_written..];
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Unexpected completion for write_static_all: {other:?}"),
                    ));
                }
            }
        }

        Ok(())
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
        // SAFETY: libc::socket has no Rust wrapper for this exact nonblocking
        // connect setup; returned fd is checked before use.
        let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fcntl operates on the valid fd created above; failures are
        // reflected by later connect/readiness handling.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        let storage = socket_addr_to_storage(&resolved);
        // SAFETY: `storage` contains a sockaddr matching `resolved`, and the
        // pointer/length pair stays valid for the duration of connect.
        let ret = unsafe {
            libc::connect(
                fd,
                &storage as *const libc::sockaddr_storage as *const libc::sockaddr,
                socket_addr_len(&resolved),
            )
        };

        if ret == 0 {
            // SAFETY: On immediate connect success, ownership of fd moves into
            // TcpStream exactly once.
            let std_stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
            return Self::new(std_stream);
        }

        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINPROGRESS) {
            // SAFETY: fd has not been wrapped in TcpStream on this error path.
            unsafe {
                libc::close(fd);
            }
            return Err(err);
        }

        // SAFETY: EINPROGRESS leaves fd open and owned by this function; move
        // ownership into TcpStream once while io_uring waits for completion.
        let std_stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
        let stream = Self::new(std_stream)?;

        let op = crate::io::Op::Connect { fd, addr: resolved };
        let token = with_io_state(|state| state.io_backend.submit(op));
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
        with_io_state(|state| unsafe {
            // SAFETY: Single-threaded access.
            if let Some(token) = self.read_token {
                state.clear_slot(token);
            }
            if let Some(token) = self.write_token {
                state.clear_slot(token);
            }
            if let Some(token) = self.pollout_token {
                state.clear_slot(token);
            }
        });
        std::mem::forget(self);
        fd
    }
}

impl Drop for AsyncTcpStream {
    fn drop(&mut self) {
        try_with_io_state(|state| unsafe {
            // SAFETY: Single-threaded access.
            if let Some(token) = self.read_token {
                state.clear_slot(token);
            }
            if let Some(token) = self.write_token {
                state.clear_slot(token);
            }
            if let Some(token) = self.pollout_token {
                state.clear_slot(token);
            }
        });
    }
}

impl AsyncRead for AsyncTcpStream {
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        with_io_state(|state| {
            // SAFETY: AsyncTcpStream is Unpin in practice because it contains no
            // self-referential fields; poll never moves fields after projection.
            let this = unsafe { self.as_mut().get_unchecked_mut() };

            if this.read_token.is_none() {
                match try_read(this.inner.as_raw_fd(), buf) {
                    Ok(bytes_read) => return Poll::Ready(Ok(bytes_read)),
                    Err(err) if is_wouldblock(&err) => {}
                    Err(err) => return Poll::Ready(Err(err)),
                }
            }

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
                let token = match state.io_backend.submit(op) {
                    Ok(token) => token,
                    Err(error) => return Poll::Ready(Err(error.into())),
                };
                this.read_token = Some(token);
                token
            };
            // Register waker AND pending_task for inline completion.
            // SAFETY: Single-threaded access.
            unsafe {
                state.store_waker_and_pending_task(token, cx.waker(), current_task_id());
            }
            Poll::Pending
        })
    }

    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [std::io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        if bufs.len() == 1 {
            let buf = &mut bufs[0];
            return AsyncRead::poll_read(self, cx, &mut *buf);
        }

        with_io_state(|state| {
            // SAFETY: AsyncTcpStream has no self-referential fields; poll only
            // mutates fields in place and never moves them out of the pin.
            let this = unsafe { self.as_mut().get_unchecked_mut() };

            if !bufs.is_empty() && this.read_token.is_none() {
                match try_read_vectored(this.inner.as_raw_fd(), bufs) {
                    Ok(bytes_read) => return Poll::Ready(Ok(bytes_read)),
                    Err(err) if is_wouldblock(&err) => {}
                    Err(err) => return Poll::Ready(Err(err)),
                }
            }

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
                let mut buffers: SmallVec<[crate::Buffer; 4]> = SmallVec::new();
                for buf in bufs {
                    buffers.push(crate::Buffer::with_capacity(buf.len()));
                }
                let op = crate::io::Op::ReadVectored {
                    fd: this.inner.as_raw_fd(),
                    buffers,
                };
                let token = match state.io_backend.submit(op) {
                    Ok(token) => token,
                    Err(error) => return Poll::Ready(Err(error.into())),
                };
                this.read_token = Some(token);
                token
            };
            // SAFETY: Single-threaded access.
            unsafe {
                state.store_waker_and_pending_task(token, cx.waker(), current_task_id());
            }
            Poll::Pending
        })
    }
}

impl AsyncWrite for AsyncTcpStream {
    #[inline]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        with_io_state(|state| {
            // SAFETY: AsyncTcpStream has no self-referential fields; poll only
            // mutates fields in place and never moves them out of the pin.
            let this = unsafe { self.as_mut().get_unchecked_mut() };

            // Clear stale POLLOUT completion (fire-and-forget).
            // POLLOUT just serves as a wakeup mechanism when the socket
            // becomes writable after an EAGAIN; the completion itself is
            // discarded, then poll_write submits a fresh Write SQE.
            if let Some(pollout_tok) = this.pollout_token.take() {
                // SAFETY: pollout_tok belongs to this stream and is being cleared
                // once before submitting/reusing any write state.
                unsafe {
                    state.clear_slot(pollout_tok);
                }
            }

            if this.write_token.is_none() {
                match try_write(this.inner.as_raw_fd(), buf) {
                    Ok(bytes_written) => return Poll::Ready(Ok(bytes_written)),
                    Err(err) if is_wouldblock(&err) => {}
                    Err(err) => return Poll::Ready(Err(err)),
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
                            let pollout_tok = match state.io_backend.submit(pollout_op) {
                                Ok(token) => token,
                                Err(error) => return Poll::Ready(Err(error.into())),
                            };
                            this.pollout_token = Some(pollout_tok);
                            // SAFETY: pollout_tok was just submitted on this
                            // shard; waker/task association is shard-local.
                            unsafe {
                                state.store_waker_and_pending_task(
                                    pollout_tok,
                                    cx.waker(),
                                    current_task_id(),
                                );
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
                let token = match state.io_backend.submit(op) {
                    Ok(token) => token,
                    Err(error) => return Poll::Ready(Err(error.into())),
                };
                this.write_token = Some(token);
                token
            };
            // SAFETY: Single-threaded access.
            unsafe {
                state.store_waker_and_pending_task(token, cx.waker(), current_task_id());
            }
            Poll::Pending
        })
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        if bufs.len() == 1 {
            let buf = bufs[0];
            return AsyncWrite::poll_write(self, cx, &buf);
        }
        if bufs.is_empty() {
            return Poll::Ready(Ok(0));
        }

        with_io_state(|state| {
            // SAFETY: AsyncTcpStream is Unpin in practice because it contains no
            // self-referential fields; poll never moves fields after projection.
            let this = unsafe { self.as_mut().get_unchecked_mut() };

            // Clear stale POLLOUT completion (fire-and-forget).
            if let Some(pollout_tok) = this.pollout_token.take() {
                // SAFETY: pollout_tok belongs to this stream and is being cleared
                // once before submitting/reusing any write state.
                unsafe {
                    state.clear_slot(pollout_tok);
                }
            }

            if !bufs.is_empty() && this.write_token.is_none() {
                match try_write_vectored(this.inner.as_raw_fd(), bufs) {
                    Ok(bytes_written) => return Poll::Ready(Ok(bytes_written)),
                    Err(err) if is_wouldblock(&err) => {}
                    Err(err) => return Poll::Ready(Err(err)),
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
                            let pollout_tok = match state.io_backend.submit(pollout_op) {
                                Ok(token) => token,
                                Err(error) => return Poll::Ready(Err(error.into())),
                            };
                            this.pollout_token = Some(pollout_tok);
                            // SAFETY: pollout_tok was just submitted on this
                            // shard; waker/task association is shard-local.
                            unsafe {
                                state.store_waker_and_pending_task(
                                    pollout_tok,
                                    cx.waker(),
                                    current_task_id(),
                                );
                            }
                            return Poll::Pending;
                        }
                        Err(e) => Poll::Ready(Err(e.into())),
                    };
                }
                token
            } else {
                let mut buffers: SmallVec<[crate::Buffer; 4]> = SmallVec::new();
                for buf in bufs {
                    buffers.push(crate::Buffer::from_slice(buf));
                }
                let op = crate::io::Op::WriteVectored {
                    fd: this.inner.as_raw_fd(),
                    buffers,
                };
                let token = match state.io_backend.submit(op) {
                    Ok(token) => token,
                    Err(error) => return Poll::Ready(Err(error.into())),
                };
                this.write_token = Some(token);
                token
            };
            // SAFETY: Single-threaded access.
            unsafe {
                state.store_waker_and_pending_task(token, cx.waker(), current_task_id());
            }
            Poll::Pending
        })
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Poll::Ready(this.inner.shutdown(Shutdown::Write))
    }
}

impl HyperRead for AsyncTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        with_io_state(|state| {
            let this = self.as_mut().get_mut();
            let raw = unsafe { buf.as_mut() };
            let read_buf =
                unsafe { std::slice::from_raw_parts_mut(raw.as_mut_ptr().cast::<u8>(), raw.len()) };

            if this.read_token.is_none() {
                match try_read(this.inner.as_raw_fd(), read_buf) {
                    Ok(bytes_read) => {
                        // SAFETY: `try_read` initialized exactly `bytes_read` bytes.
                        unsafe {
                            buf.advance(bytes_read);
                        }
                        return Poll::Ready(Ok(()));
                    }
                    Err(err) if is_wouldblock(&err) => {}
                    Err(err) => return Poll::Ready(Err(err)),
                }
            }

            let token = if let Some(token) = this.read_token {
                // SAFETY: Single-threaded access.
                if let Some(result) = unsafe { state.take_completed(token) } {
                    this.read_token = None;
                    return match result {
                        Ok(crate::io::CompletionKind::Read { bytes_read, data }) => {
                            let copy_len = std::cmp::min(bytes_read, buf.remaining());
                            if copy_len > 0 {
                                buf.put_slice(&data[..copy_len]);
                            }
                            Poll::Ready(Ok(()))
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
                    len: buf.remaining(),
                };
                let token = match state.io_backend.submit(op) {
                    Ok(token) => token,
                    Err(error) => return Poll::Ready(Err(error.into())),
                };
                this.read_token = Some(token);
                token
            };
            // SAFETY: Single-threaded access.
            unsafe {
                state.store_waker_and_pending_task(token, cx.waker(), current_task_id());
            }
            Poll::Pending
        })
    }
}

impl HyperWrite for AsyncTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(self, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(self, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Poll::Ready(this.inner.shutdown(Shutdown::Write))
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write_vectored(self, cx, bufs)
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
        // SAFETY: setsockopt only reads the provided size value during the
        // call; fd belongs to `socket` and remains open.
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
        let op = crate::io::Op::UdpRecv {
            fd: self.inner.as_raw_fd(),
            // Kernel writes into this owned buffer, so zero-fill is wasted work.
            buffer: Buffer::new_uninitialized(buf.len()),
        };
        let token = with_io_state(|state| state.io_backend.submit(op));
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

    /// Receive a UDP datagram into an owned buffer and return the same buffer
    /// after completion.
    pub async fn recv_from_rent<T>(&self, buffer: T) -> io::Result<(usize, SocketAddr, T)>
    where
        T: RentBuffer,
    {
        let op = crate::io::Op::UdpRecv {
            fd: self.inner.as_raw_fd(),
            buffer: buffer.into(),
        };
        let token = with_io_state(|state| state.io_backend.submit(op));
        let future = crate::io::future::IoFuture::new(token);
        match future.await? {
            crate::io::CompletionKind::UdpRecv {
                bytes_read,
                buffer,
                addr,
            } => Ok((bytes_read, addr, T::from(buffer))),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion",
            )),
        }
    }

    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        let op = crate::io::Op::UdpSend {
            fd: self.inner.as_raw_fd(),
            data: Buffer::from_slice(buf),
            addr: target,
        };
        let token = with_io_state(|state| state.io_backend.submit(op));
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
    // SAFETY: sockaddr_storage is a plain C storage type; it is immediately
    // overwritten with the matching sockaddr_in/sockaddr_in6 below.
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
            // SAFETY: storage is large/aligned enough for sockaddr_in.
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
            // SAFETY: storage is large/aligned enough for sockaddr_in6.
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

#[inline]
fn is_wouldblock(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::WouldBlock
        || err.raw_os_error() == Some(libc::EAGAIN)
        || err.raw_os_error() == Some(libc::EWOULDBLOCK)
}

#[inline]
fn try_read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: `buf` points to a valid writable slice for the duration of
        // this synchronous nonblocking syscall.
        let n = unsafe {
            libc::read(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len() as libc::size_t,
            )
        };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

#[inline]
fn try_read_vectored(fd: RawFd, bufs: &mut [std::io::IoSliceMut<'_>]) -> io::Result<usize> {
    if bufs.is_empty() {
        return Ok(0);
    }
    loop {
        match bufs.len() {
            1 => return try_read(fd, &mut bufs[0]),
            2 => {
                let iovecs = [
                    libc::iovec {
                        iov_base: bufs[0].as_mut_ptr() as *mut libc::c_void,
                        iov_len: bufs[0].len() as libc::size_t,
                    },
                    libc::iovec {
                        iov_base: bufs[1].as_mut_ptr() as *mut libc::c_void,
                        iov_len: bufs[1].len() as libc::size_t,
                    },
                ];
                // SAFETY: `iovecs` contains writable buffers derived from the
                // caller's mutable slices and remains valid for this syscall.
                let n = unsafe { libc::readv(fd, iovecs.as_ptr(), iovecs.len() as libc::c_int) };
                if n >= 0 {
                    return Ok(n as usize);
                }
            }
            _ => {
                let mut iovecs: SmallVec<[libc::iovec; 4]> = SmallVec::new();
                for buf in bufs.iter_mut() {
                    iovecs.push(libc::iovec {
                        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
                        iov_len: buf.len() as libc::size_t,
                    });
                }
                // SAFETY: `iovecs` contains writable buffers derived from the
                // caller's mutable slices and remains valid for this syscall.
                let n = unsafe { libc::readv(fd, iovecs.as_ptr(), iovecs.len() as libc::c_int) };
                if n >= 0 {
                    return Ok(n as usize);
                }
            }
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

#[inline]
fn try_write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    loop {
        // SAFETY: `buf` points to a valid readable slice for the duration of
        // this synchronous nonblocking syscall.
        let n = unsafe {
            libc::write(
                fd,
                buf.as_ptr() as *const libc::c_void,
                buf.len() as libc::size_t,
            )
        };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

#[inline]
fn try_write_vectored(fd: RawFd, bufs: &[std::io::IoSlice<'_>]) -> io::Result<usize> {
    if bufs.is_empty() {
        return Ok(0);
    }
    loop {
        match bufs.len() {
            1 => return try_write(fd, bufs[0].as_ref()),
            2 => {
                let iovecs = [
                    libc::iovec {
                        iov_base: bufs[0].as_ptr() as *mut libc::c_void,
                        iov_len: bufs[0].len() as libc::size_t,
                    },
                    libc::iovec {
                        iov_base: bufs[1].as_ptr() as *mut libc::c_void,
                        iov_len: bufs[1].len() as libc::size_t,
                    },
                ];
                // SAFETY: `iovecs` contains readable buffers derived from the
                // caller's borrowed slices and remains valid for this syscall.
                let n = unsafe { libc::writev(fd, iovecs.as_ptr(), iovecs.len() as libc::c_int) };
                if n >= 0 {
                    return Ok(n as usize);
                }
            }
            _ => {
                let mut iovecs: SmallVec<[libc::iovec; 4]> = SmallVec::new();
                for buf in bufs {
                    iovecs.push(libc::iovec {
                        iov_base: buf.as_ptr() as *mut libc::c_void,
                        iov_len: buf.len() as libc::size_t,
                    });
                }
                // SAFETY: `iovecs` contains readable buffers derived from the
                // caller's borrowed slices and remains valid for this syscall.
                let n = unsafe { libc::writev(fd, iovecs.as_ptr(), iovecs.len() as libc::c_int) };
                if n >= 0 {
                    return Ok(n as usize);
                }
            }
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
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
