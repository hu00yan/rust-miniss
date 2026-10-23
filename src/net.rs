use crate::cpu::{current_task_id, try_with_io_state, with_io_state, CpuIoState};
use crate::io::{DescriptorLease, IoToken, RegisteredDescriptor};
use crate::{Buffer, RentBuffer};
use futures::io::{AsyncRead, AsyncWrite};
use hyper::rt::{Read as HyperRead, ReadBufCursor, Write as HyperWrite};
use libc;
use smallvec::SmallVec;
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::marker::PhantomData;
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::OwnedFd;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::pin::Pin;
use std::rc::Rc;
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
        let result = await_accept(&mut self.accept_token, token).await;
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

/// Keeps the cached accept token only when the wait completes normally.
/// Architecture audit: the cancellation/connect/write repairs add only
/// listener-local state, per-socket options, and syscall flags—no shared
/// mutable state, locks, atomics, runtime creation, blocking calls, or
/// cross-core traffic.
struct AcceptTokenGuard<'a> {
    token: &'a mut Option<IoToken>,
    keep: bool,
}

impl Drop for AcceptTokenGuard<'_> {
    fn drop(&mut self) {
        if !self.keep {
            *self.token = None;
        }
    }
}

async fn await_accept(
    cached_token: &mut Option<IoToken>,
    token: IoToken,
) -> Result<crate::io::CompletionKind, crate::io::IoError> {
    let mut guard = AcceptTokenGuard {
        token: cached_token,
        keep: false,
    };
    let result = crate::io::future::IoFuture::accept(token).await;
    guard.keep = true;
    result
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
    socket.listen(crate::io::ACCEPT_BACKLOG_CAPACITY as i32)?;
    Ok(socket.into())
}

/// An asynchronous TCP stream.
///
/// # Buffer ownership
///
/// The three shapes are kept apart on purpose, so a caller can tell from the
/// signature alone whether it keeps its buffer or hands it over:
///
/// | method | buffer | copy | who holds the bytes while a call blocks |
/// |---|---|---|---|
/// | `read_rent`, `write_rent` | **rented**: the caller hands over an owned `T: RentBuffer` and gets the same allocation back | none beyond the `T`/`Buffer` conversion | the backend op, on the owning shard |
/// | `poll_read`, `poll_read_vectored`, `poll_write`, `poll_write_vectored` | **borrowed**: the caller's slices | none — a direct `read(2)`/`write(2)`/`readv(2)`/`writev(2)`, retried on the readiness wakeup | nobody: the syscall only runs inside `poll`, so a queued op can never outlive the caller's buffer |
/// | `write_all_static` | borrowed `&'static [u8]` | none — tries `write(2)`, then queues `Op::WriteStatic`, which points the backend straight at those bytes | the backend op; `'static` is exactly why queueing it is sound |
///
/// Writing a head followed by a body is an HTTP response detail, not a stream
/// capability, so `HttpConnection::send_response` does that itself.
#[derive(Debug)]
pub struct AsyncTcpStream {
    descriptor: Option<RegisteredDescriptor>,
    readiness_token: Option<IoToken>,
    /// Token for the POLLOUT registration submitted when `poll_write` or
    /// `poll_write_vectored` finds the send buffer full. The task is woken when
    /// the socket may be writable and retries the syscall directly; no write is
    /// queued and no byte is copied.
    pollout_token: Option<IoToken>,
    /// Stream I/O tokens and TLS state belong to the creating shard. The
    /// marker prevents accidental migration through an `Arc<OwnedFd>` lease.
    _owner_local: PhantomData<Rc<()>>,
}

impl AsyncTcpStream {
    #[inline]
    fn descriptor_lease(&self) -> DescriptorLease {
        self.descriptor
            .as_ref()
            .expect("live stream must own its descriptor")
            .lease()
    }

    /// Creates a new AsyncTcpStream from a TcpStream.
    pub fn new(stream: TcpStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Self::from_nonblocking(stream)
    }

    /// Creates a new AsyncTcpStream from a TcpStream that is already non-blocking.
    fn from_nonblocking(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        #[cfg(target_os = "macos")]
        {
            let enabled: libc::c_int = 1;
            // SAFETY: `stream` owns a live socket and `enabled` is valid for
            // the duration of this setsockopt call.
            let result = unsafe {
                libc::setsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_NOSIGPIPE,
                    &enabled as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&enabled) as libc::socklen_t,
                )
            };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let descriptor = RegisteredDescriptor::from_owned(OwnedFd::from(stream));
        Ok(Self {
            descriptor: Some(descriptor),
            readiness_token: None,
            pollout_token: None,
            _owner_local: PhantomData,
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
        let fd = self.as_raw_fd();
        let mut buffer = buffer.into();
        let direct = with_io_state(|state| state.io_backend.uses_completion_io());
        match if direct {
            Err(io::Error::from_raw_os_error(libc::EAGAIN))
        } else {
            // SAFETY: The owned buffer provides a writable region up to its full
            // capacity; the slice is only used for this synchronous syscall.
            try_read(fd, unsafe {
                std::slice::from_raw_parts_mut(buffer.as_mut_ptr(), buffer.capacity())
            })
        } {
            Ok(bytes_read) => {
                // SAFETY: `try_read` initialized exactly `bytes_read` bytes.
                unsafe {
                    buffer.set_len(bytes_read);
                }
                Ok((bytes_read, T::from(buffer)))
            }
            Err(err) if is_wouldblock(&err) => {
                let op = crate::io::Op::ReadRent { fd, buffer };
                let descriptor = self.descriptor_lease();
                let token =
                    with_io_state(|state| state.io_backend.submit_with_descriptor(op, descriptor));
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
        let fd = self.as_raw_fd();
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
                let descriptor = self.descriptor_lease();
                let token =
                    with_io_state(|state| state.io_backend.submit_with_descriptor(op, descriptor));
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

    /// Write a static byte slice without copying it into an owned Buffer.
    pub async fn write_all_static(&self, mut data: &'static [u8]) -> io::Result<()> {
        while !data.is_empty() {
            // Tiny static responses usually fit the socket send buffer. Use the
            // nonblocking fast path first to avoid an avoidable SQE/CQE round
            // trip; EAGAIN still falls back to the owner-thread io_uring path.
            // Set MINISS_NO_STATIC_TRY_WRITE=1 for controlled comparison.
            if std::env::var_os("MINISS_NO_STATIC_TRY_WRITE").is_none() {
                match try_write(self.as_raw_fd(), data) {
                    Ok(bytes_written) if bytes_written > 0 => {
                        data = &data[bytes_written..];
                        continue;
                    }
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "write_all_static wrote zero bytes",
                        ));
                    }
                    Err(error) if is_wouldblock(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            let op = crate::io::Op::WriteStatic {
                fd: self.as_raw_fd(),
                data,
            };
            let descriptor = self
                .descriptor
                .as_ref()
                .expect("live stream must retain its descriptor registration")
                .lease();
            let token =
                with_io_state(|state| state.io_backend.submit_with_descriptor(op, descriptor));
            let future = crate::io::future::IoFuture::new(token);
            match future.await? {
                crate::io::CompletionKind::Write { bytes_written } => {
                    if bytes_written == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "write_all_static wrote zero bytes",
                        ));
                    }
                    data = &data[bytes_written..];
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Unexpected completion for write_all_static: {other:?}"),
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

        // Architecture audit: use only the already-installed shard backend;
        // this adds no shared mutable state, locks, runtime creation, blocking
        // calls, or cross-core traffic.
        let completion_io =
            try_with_io_state(|state| state.io_backend.uses_completion_io()).unwrap_or(false);
        if !completion_io {
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
        }

        // For completion backends, leave the socket unconnected and let the
        // backend's Connect operation initiate it. Readiness backends arrive
        // here only after libc::connect returned EINPROGRESS.
        // SAFETY: fd remains open and owned by this function; move ownership
        // into TcpStream exactly once while the backend completes the connect.
        let std_stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
        let stream = Self::new(std_stream)?;

        let op = crate::io::Op::Connect { fd, addr: resolved };
        let descriptor = stream.descriptor_lease();
        let token = with_io_state(|state| state.io_backend.submit_with_descriptor(op, descriptor));
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
        self.descriptor
            .as_ref()
            .expect("live stream must own its descriptor")
            .as_raw_fd()
    }
}

impl IntoRawFd for AsyncTcpStream {
    fn into_raw_fd(mut self) -> RawFd {
        // SAFETY: `state` is this thread's own shard, and the readiness tokens
        // belong to this stream — `cancel_and_discard`'s contract is met.
        with_io_state(|state| unsafe {
            if let Some(token) = self.readiness_token {
                state.cancel_and_discard(token);
            }
            if let Some(token) = self.pollout_token {
                state.cancel_and_discard(token);
            }
        });
        self.descriptor
            .take()
            .expect("live stream must own its descriptor")
            .into_raw_fd()
            .expect("failed to transfer stream descriptor")
    }
}

impl Drop for AsyncTcpStream {
    fn drop(&mut self) {
        // SAFETY: as in `into_raw_fd` — own shard, own tokens. `try_with_io_state`
        // just skips the body when the dropping thread has no runtime state.
        try_with_io_state(|state| unsafe {
            if let Some(token) = self.readiness_token {
                state.cancel_and_discard(token);
            }
            if let Some(token) = self.pollout_token {
                state.cancel_and_discard(token);
            }
        });
    }
}

/// One readiness attempt, shared by every borrowed (non-rent) poll path.
///
/// All five call sites — scalar and vectored, read and write, plus
/// `HyperRead::poll_read` — do the same four things in the same order, so the
/// order only has to be correct here:
///
/// 1. consume the readiness completion that woke us. It carries no payload; it
///    means "run the syscall again", nothing more;
/// 2. run that syscall directly. A queued op could outlive the caller's buffer,
///    so the syscall only ever runs inside `poll`;
/// 3. on `WouldBlock`, arm `events` on `fd` and register this task's waker, so
///    the runtime re-enters `poll` when the descriptor is ready;
/// 4. otherwise return the result.
///
/// `slot` is the caller's own registration handle for *its* direction
/// (`readiness_token` for reads, `pollout_token` for writes), so the two
/// directions can never consume each other's registration.
fn poll_readiness(
    state: &CpuIoState,
    fd: RawFd,
    descriptor: DescriptorLease,
    cx: &Context<'_>,
    events: u32,
    slot: &mut Option<IoToken>,
    attempt: impl FnOnce() -> io::Result<usize>,
) -> Poll<io::Result<usize>> {
    if let Some(token) = slot.take() {
        // Consume a completion already delivered to the task before attempting
        // to cancel; completion-based backends retain discarded slots until
        // terminal completion, which has already happened in this case.
        // SAFETY: `token` belongs to this shard and the poll owns its slot.
        let completed = unsafe { state.has_completed(token) }
            || unsafe { state.take_owner_completion(token) }.is_some();
        if completed {
            // SAFETY: same live shard-local token; its completion is consumed.
            unsafe { state.clear_slot(token) };
        } else if state.io_backend.uses_completion_io() {
            // SAFETY: `token` was submitted on this shard; async cancellation
            // must retain its slot until the backend reports terminal completion.
            unsafe { state.cancel_and_discard(token) };
        } else {
            // Readiness backends cancel synchronously, releasing the registration
            // and descriptor lease before a replacement PollAdd is submitted.
            let _ = state.io_backend.cancel(token);
            // SAFETY: this token was submitted on this shard and synchronous
            // cancellation (or prior backend retirement) makes its slot reusable.
            unsafe { state.clear_slot(token) };
        }
    }

    match attempt() {
        Ok(bytes) => return Poll::Ready(Ok(bytes)),
        Err(err) if is_wouldblock(&err) => {}
        Err(err) => return Poll::Ready(Err(err)),
    }

    let op = crate::io::Op::PollAdd {
        fd,
        events,
        multishot: false,
    };
    let token = match state.io_backend.submit_with_descriptor(op, descriptor) {
        Ok(token) => token,
        Err(error) => return Poll::Ready(Err(error.into())),
    };
    *slot = Some(token);
    // Register waker AND pending_task for inline completion.
    // SAFETY: `token` was just submitted on this shard; the waker/task
    // association is shard-local.
    unsafe {
        state.store_waker_and_pending_task(token, cx.waker(), current_task_id());
    }
    Poll::Pending
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
            let this = self.as_mut().get_mut();
            let fd = this.as_raw_fd();
            let descriptor = this.descriptor_lease();
            poll_readiness(
                state,
                fd,
                descriptor,
                cx,
                libc::POLLIN as u32,
                &mut this.readiness_token,
                || try_read(fd, buf),
            )
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
            let this = self.as_mut().get_mut();
            let fd = this.as_raw_fd();
            let descriptor = this.descriptor_lease();
            // readv straight into the caller's slices: one syscall, one copy.
            // Submitting `Op::ReadVectored` would have the backend allocate its
            // own `Buffer`s, read into them, and then be copied a second time
            // into `bufs` on completion.
            poll_readiness(
                state,
                fd,
                descriptor,
                cx,
                libc::POLLIN as u32,
                &mut this.readiness_token,
                || try_read_vectored(fd, bufs),
            )
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
            let this = self.as_mut().get_mut();
            let fd = this.as_raw_fd();
            let descriptor = this.descriptor_lease();
            // write(2) straight from the caller's slice: one syscall, no copy.
            // Submitting `Op::Write` would make the backend copy `buf` into an
            // owned `Buffer` and queue that, and `buf` is borrowed, so it could
            // not be handed to the kernel as a queued operation anyway.
            // `poll_read` reasons the same way about the borrowed read buffer.
            poll_readiness(
                state,
                fd,
                descriptor,
                cx,
                libc::POLLOUT as u32,
                &mut this.pollout_token,
                || try_write(fd, buf),
            )
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
            let this = self.as_mut().get_mut();
            let fd = this.as_raw_fd();
            let descriptor = this.descriptor_lease();
            // writev(2) straight from the caller's slices: one syscall, no copy —
            // the mirror of `poll_read_vectored`. The queued-vectored shape this
            // replaced had the backend copy every slice into its own `Buffer`s and
            // then write those, which is the same double copy the readv path
            // avoids; it was retired with `Op::WriteVectored`.
            poll_readiness(
                state,
                fd,
                descriptor,
                cx,
                libc::POLLOUT as u32,
                &mut this.pollout_token,
                || try_write_vectored(fd, bufs),
            )
        })
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // SAFETY: the descriptor is live for the duration of this call.
        let result = unsafe { libc::shutdown(this.as_raw_fd(), libc::SHUT_WR) };
        if result == 0 {
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(io::Error::last_os_error()))
        }
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
            // SAFETY: `ReadBufCursor` exposes the unwritten tail of hyper's
            // buffer as a writable chunk for the duration of this call.
            let raw = unsafe { buf.as_mut() };
            let read_buf =
                // SAFETY: same region, `raw.len()` is its exact capacity.
                unsafe { std::slice::from_raw_parts_mut(raw.as_mut_ptr().cast::<u8>(), raw.len()) };
            let fd = this.as_raw_fd();
            let descriptor = this.descriptor_lease();

            match poll_readiness(
                state,
                fd,
                descriptor,
                cx,
                libc::POLLIN as u32,
                &mut this.readiness_token,
                || try_read(fd, read_buf),
            ) {
                Poll::Ready(Ok(bytes_read)) => {
                    // SAFETY: `try_read` initialized exactly `bytes_read` bytes.
                    unsafe {
                        buf.advance(bytes_read);
                    }
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
                Poll::Pending => Poll::Pending,
            }
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

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_close(self, cx)
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

    /// Send a datagram from an owned buffer and get that same buffer back.
    ///
    /// The send-side mirror of [`Self::recv_from_rent`]: the caller hands over
    /// an owned buffer, the runtime gives it to the kernel, and completion
    /// returns the allocation rather than a copy of it. As with the other rent
    /// operations, a queued datagram that completes with an error drops the
    /// buffer with the op.
    pub async fn send_to_rent<T>(&self, data: T, target: SocketAddr) -> io::Result<(usize, T)>
    where
        T: RentBuffer,
    {
        let op = crate::io::Op::UdpSend {
            fd: self.inner.as_raw_fd(),
            data: data.into(),
            addr: target,
        };
        let token = with_io_state(|state| state.io_backend.submit(op));
        let future = crate::io::future::IoFuture::new(token);
        match future.await? {
            crate::io::CompletionKind::UdpSend {
                bytes_written,
                data,
            } => Ok((bytes_written, T::from(data))),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unexpected completion for send_to_rent: {other:?}"),
            )),
        }
    }

    /// Send a datagram from a borrowed slice.
    ///
    /// A queued datagram has to own its payload, so this pays one copy into an
    /// owned buffer; callers that already hold an owned buffer should use
    /// [`Self::send_to_rent`] and pay none.
    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        let (bytes_written, _) = self.send_to_rent(Buffer::from_slice(buf), target).await?;
        Ok(bytes_written)
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
            #[cfg(target_os = "linux")]
            {
                libc::send(
                    fd,
                    buf.as_ptr() as *const libc::c_void,
                    buf.len() as libc::size_t,
                    libc::MSG_NOSIGNAL,
                )
            }
            #[cfg(not(target_os = "linux"))]
            {
                libc::write(
                    fd,
                    buf.as_ptr() as *const libc::c_void,
                    buf.len() as libc::size_t,
                )
            }
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
fn try_writev_once(fd: RawFd, iovecs: &[libc::iovec]) -> isize {
    // SAFETY: `iovecs` points to caller-owned readable slices that remain valid
    // for this synchronous syscall, and the message header is initialized below.
    unsafe {
        #[cfg(target_os = "linux")]
        {
            let mut message: libc::msghdr = std::mem::zeroed();
            message.msg_iov = iovecs.as_ptr() as *mut libc::iovec;
            message.msg_iovlen = iovecs.len() as _;
            libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL)
        }
        #[cfg(not(target_os = "linux"))]
        {
            libc::writev(fd, iovecs.as_ptr(), iovecs.len() as libc::c_int)
        }
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
                let n = try_writev_once(fd, &iovecs);
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
                let n = try_writev_once(fd, &iovecs);
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

#[cfg(all(test, target_os = "linux"))]
mod completion_connect_tests {
    use super::{socket_addr_len, socket_addr_to_storage, AsyncTcpStream};
    use crate::cpu::{test_io_state, RuntimeContextGuard};
    use crate::io::{CompletionKind, IoError, IoProvider, IoToken, Op};
    use crate::timer::TimerWheel;
    use std::cell::Cell;
    use std::future::Future;
    use std::net::TcpListener;
    use std::rc::Rc;
    use std::task::{Context, Poll};

    struct ConnectBackend {
        submissions: Rc<Cell<usize>>,
        syscall_result: Rc<Cell<Option<(i32, Option<i32>)>>>,
        socket_state_before_submit: Rc<Cell<Option<u8>>>,
    }

    impl IoProvider for ConnectBackend {
        type Completion = (IoToken, Result<CompletionKind, IoError>);

        fn submit(&self, op: Op) -> Result<IoToken, IoError> {
            let Op::Connect { fd, addr } = op else {
                return Err(IoError::Other("connect expected".into()));
            };
            let mut tcp_info = [0_u8; 256];
            let mut tcp_info_len = tcp_info.len() as libc::socklen_t;
            // SAFETY: `tcp_info` is a writable output buffer for getsockopt.
            let info_result = unsafe {
                libc::getsockopt(
                    fd,
                    libc::IPPROTO_TCP,
                    libc::TCP_INFO,
                    tcp_info.as_mut_ptr().cast(),
                    &mut tcp_info_len,
                )
            };
            if info_result < 0 {
                return Err(IoError::Io(std::io::Error::last_os_error()));
            }
            self.socket_state_before_submit.set(Some(tcp_info[0]));
            let storage = socket_addr_to_storage(&addr);
            // Simulate the completion backend's Connect SQE by initiating the
            // connection at submission, and record the result for the test.
            // SAFETY: `fd` is the live socket owned by the connect future, and
            // `storage` contains a matching sockaddr valid through this call.
            let result = unsafe {
                libc::connect(
                    fd,
                    &storage as *const libc::sockaddr_storage as *const libc::sockaddr,
                    socket_addr_len(&addr),
                )
            };
            let error =
                (result < 0).then(|| std::io::Error::last_os_error().raw_os_error().unwrap_or(-1));
            self.submissions.set(self.submissions.get() + 1);
            self.syscall_result.set(Some((result, error)));
            Ok(IoToken::new())
        }

        fn uses_completion_io(&self) -> bool {
            true
        }

        fn drain_completions(&self) -> Poll<Vec<Self::Completion>> {
            Poll::Pending
        }

        fn cancel(&self, _token: IoToken) -> bool {
            true
        }
    }

    #[test]
    fn completion_backend_initiates_connect_exactly_once() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let submissions = Rc::new(Cell::new(0));
        let syscall_result = Rc::new(Cell::new(None));
        let socket_state_before_submit = Rc::new(Cell::new(None));
        let io_state = test_io_state(ConnectBackend {
            submissions: submissions.clone(),
            syscall_result: syscall_result.clone(),
            socket_state_before_submit: socket_state_before_submit.clone(),
        });
        let mut timers = TimerWheel::default();
        // SAFETY: the test keeps the state and timer on this owner thread
        // until the guard is dropped.
        let _guard = unsafe { RuntimeContextGuard::enter(&io_state, &mut timers, Some(0)) };
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut connect = Box::pin(AsyncTcpStream::connect(addr));
        assert!(matches!(connect.as_mut().poll(&mut cx), Poll::Pending));
        assert_eq!(submissions.get(), 1);
        assert_eq!(
            socket_state_before_submit.get(),
            Some(7), // Linux TCP_CLOSE: completion backends receive an unconnected socket.
            "libc::connect must not have started the connection before backend submission"
        );
        let (result, error) = syscall_result.get().expect("backend connect syscall");
        assert!(
            result == 0 || error == Some(libc::EINPROGRESS),
            "the backend must initiate a fresh connection, not retry an in-flight one: \
             result={result}, error={error:?}"
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_sigpipe_tests {
    use super::AsyncTcpStream;
    use std::net::TcpStream;
    use std::os::fd::{AsRawFd, FromRawFd};

    #[test]
    fn created_tcp_streams_disable_sigpipe() {
        // SAFETY: the successful socket call transfers ownership of this fd
        // into the TcpStream immediately below.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
        // SAFETY: the successful socket call transferred this fresh descriptor
        // to the test, and TcpStream takes ownership exactly once here.
        let client = unsafe { TcpStream::from_raw_fd(fd) };
        let stream = AsyncTcpStream::new(client).unwrap();
        let mut enabled: libc::c_int = 0;
        let mut length = std::mem::size_of_val(&enabled) as libc::socklen_t;
        // SAFETY: `stream` owns a live socket and the output arguments are
        // writable for the duration of getsockopt.
        assert_eq!(
            unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_NOSIGPIPE,
                    &mut enabled as *mut _ as *mut libc::c_void,
                    &mut length,
                )
            },
            0
        );
        assert_eq!(enabled, 1);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod sigpipe_tests {
    use super::{try_write, try_write_vectored};
    use std::io::IoSlice;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::Command;

    const CHILD_MODE: &str = "RUST_MINISS_SIGPIPE_CHILD_MODE";

    #[test]
    fn borrowed_write_syscalls_do_not_raise_sigpipe() {
        if let Some(mode) = std::env::var_os(CHILD_MODE) {
            // SAFETY: this runs only in the isolated child test process.
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            }
            let mut fds = [-1; 2];
            // SAFETY: `fds` provides two writable slots for socketpair.
            assert_eq!(
                unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) },
                0
            );
            // SAFETY: successful socketpair transferred ownership of each fd.
            let writer = unsafe { OwnedFd::from_raw_fd(fds[0]) };
            // SAFETY: close the peer before writing to force EPIPE/SIGPIPE.
            assert_eq!(unsafe { libc::close(fds[1]) }, 0);
            let result = match mode.to_str().expect("ASCII test mode") {
                "scalar" => try_write(writer.as_raw_fd(), b"x"),
                "vectored" => try_write_vectored(
                    writer.as_raw_fd(),
                    &[IoSlice::new(b"x"), IoSlice::new(b"y")],
                ),
                _ => panic!("unknown child mode"),
            };
            assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EPIPE));
            return;
        }

        let executable = std::env::current_exe().unwrap();
        let mut failed_modes = Vec::new();
        for mode in ["scalar", "vectored"] {
            let output = Command::new(&executable)
                .arg("borrowed_write_syscalls_do_not_raise_sigpipe")
                .env(CHILD_MODE, mode)
                .output()
                .unwrap();
            if !output.status.success() {
                failed_modes.push(mode);
            }
        }
        assert!(
            failed_modes.is_empty(),
            "isolated writes raised SIGPIPE in modes: {failed_modes:?}"
        );
    }
}

#[cfg(test)]
mod canceled_accept_tests {
    use super::AsyncTcpListener;
    use crate::cpu::{test_io_state, RuntimeContextGuard};
    use crate::io::{CompletionKind, IoError, IoProvider, IoToken, Op};
    use crate::timer::TimerWheel;
    use std::cell::Cell;
    use std::future::Future;
    use std::rc::Rc;
    use std::task::{Context, Poll};

    struct AcceptBackend {
        submissions: Rc<Cell<usize>>,
        cancellations: Rc<Cell<usize>>,
    }

    impl IoProvider for AcceptBackend {
        type Completion = (IoToken, Result<CompletionKind, IoError>);

        fn submit(&self, op: Op) -> Result<IoToken, IoError> {
            if !matches!(op, Op::Accept { .. }) {
                return Err(IoError::Other("accept expected".into()));
            }
            self.submissions.set(self.submissions.get() + 1);
            Ok(IoToken::new())
        }

        fn drain_completions(&self) -> Poll<Vec<Self::Completion>> {
            Poll::Pending
        }

        fn cancel(&self, _token: IoToken) -> bool {
            self.cancellations.set(self.cancellations.get() + 1);
            true
        }
    }

    #[test]
    fn canceled_accept_submits_a_fresh_operation_next_time() {
        let submissions = Rc::new(Cell::new(0));
        let cancellations = Rc::new(Cell::new(0));
        let io_state = test_io_state(AcceptBackend {
            submissions: submissions.clone(),
            cancellations: cancellations.clone(),
        });
        let mut timers = TimerWheel::default();
        // SAFETY: the test keeps the state and timer on this owner thread
        // until the guard is dropped.
        let _guard = unsafe { RuntimeContextGuard::enter(&io_state, &mut timers, Some(0)) };
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut first_accept = Box::pin(listener.accept());
        assert!(matches!(first_accept.as_mut().poll(&mut cx), Poll::Pending));
        assert_eq!(submissions.get(), 1);
        drop(first_accept);
        assert_eq!(cancellations.get(), 1);

        let mut next_accept = Box::pin(listener.accept());
        assert!(matches!(next_accept.as_mut().poll(&mut cx), Poll::Pending));
        assert_eq!(
            submissions.get(),
            2,
            "a canceled accept must not reuse its abandoned token"
        );
    }
}

#[cfg(test)]
mod locality_contract_tests {
    use super::AsyncTcpStream;

    static_assertions::assert_not_impl_any!(AsyncTcpStream: Send, Sync);
}

#[cfg(test)]
mod readiness_repoll_tests {
    use super::poll_readiness;
    use crate::cpu::test_io_state;
    use crate::io::{
        CompletionKind, DescriptorLease, IoError, IoProvider, IoToken, Op, RegisteredDescriptor,
    };
    use std::cell::RefCell;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::rc::Rc;
    use std::task::{Context, Poll};

    #[derive(Default)]
    struct BackendState {
        pending: Vec<PendingPoll>,
        cancellations: usize,
    }

    struct PendingPoll {
        token: IoToken,
        fd: i32,
        _descriptor: DescriptorLease,
    }

    struct DuplicatePollBackend(Rc<RefCell<BackendState>>);

    impl DuplicatePollBackend {
        fn register(&self, fd: i32, descriptor: DescriptorLease) -> Result<IoToken, IoError> {
            let mut state = self.0.borrow_mut();
            if state.pending.iter().any(|pending| pending.fd == fd) {
                return Err(IoError::Io(std::io::Error::from_raw_os_error(libc::EEXIST)));
            }
            let token = IoToken::new();
            state.pending.push(PendingPoll {
                token,
                fd,
                _descriptor: descriptor,
            });
            Ok(token)
        }
    }

    impl IoProvider for DuplicatePollBackend {
        type Completion = (IoToken, Result<CompletionKind, IoError>);

        fn submit(&self, op: Op) -> Result<IoToken, IoError> {
            let Op::PollAdd { fd, .. } = op else {
                return Err(IoError::Other("readiness poll expected".into()));
            };
            self.register(fd, DescriptorLease::acquire(fd).map_err(IoError::Io)?)
        }

        fn submit_with_descriptor(
            &self,
            op: Op,
            descriptor: DescriptorLease,
        ) -> Result<IoToken, IoError> {
            let Op::PollAdd { fd, .. } = op else {
                return Err(IoError::Other("readiness poll expected".into()));
            };
            self.register(fd, descriptor)
        }

        fn drain_completions(&self) -> Poll<Vec<Self::Completion>> {
            Poll::Pending
        }

        fn cancel(&self, token: IoToken) -> bool {
            let mut state = self.0.borrow_mut();
            let Some(index) = state
                .pending
                .iter()
                .position(|pending| pending.token == token)
            else {
                return false;
            };
            state.pending.swap_remove(index);
            state.cancellations += 1;
            true
        }

        fn pending_operation_count(&self) -> usize {
            self.0.borrow().pending.len()
        }

        fn retained_descriptor_count(&self) -> usize {
            self.0.borrow().pending.len()
        }
    }

    #[test]
    fn repoll_after_unrelated_wake_cancels_the_previous_readiness_registration() {
        let state = Rc::new(RefCell::new(BackendState::default()));
        let io_state = test_io_state(DuplicatePollBackend(state.clone()));
        let mut fds = [0; 2];
        // SAFETY: `fds` contains two writable slots for `pipe` to fill.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: the successful pipe call transferred each endpoint to this test.
        let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // SAFETY: same as above for the other endpoint.
        let _write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        let descriptor = RegisteredDescriptor::acquire(read_end.as_raw_fd()).expect("lease fd");
        let waker = futures::task::noop_waker();
        let cx = Context::from_waker(&waker);
        let mut slot = None;
        let would_block = || Err(std::io::Error::from(std::io::ErrorKind::WouldBlock));

        assert!(matches!(
            poll_readiness(
                &io_state,
                read_end.as_raw_fd(),
                descriptor.lease(),
                &cx,
                libc::POLLIN as u32,
                &mut slot,
                would_block,
            ),
            Poll::Pending
        ));
        let first_token = slot.expect("first readiness registration");

        // Model the task being polled again by an unrelated wake before this
        // readiness token has completed.
        let repoll = poll_readiness(
            &io_state,
            read_end.as_raw_fd(),
            descriptor.lease(),
            &cx,
            libc::POLLIN as u32,
            &mut slot,
            would_block,
        );
        assert!(
            matches!(repoll, Poll::Pending),
            "re-poll should replace the old registration, not return EEXIST: {repoll:?}"
        );
        let second_token = slot.expect("replacement readiness registration");

        assert_ne!(first_token, second_token);
        assert_eq!(io_state.io_backend.pending_operation_count(), 1);
        assert_eq!(io_state.io_backend.retained_descriptor_count(), 1);
        assert_eq!(state.borrow().cancellations, 1);

        assert!(io_state.io_backend.cancel(second_token));
        // SAFETY: the test owns this token on the same shard; the mock backend
        // synchronously removed its registration above.
        unsafe { io_state.clear_slot(second_token) };
    }
}
