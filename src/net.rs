use crate::cpu::{try_with_io_state, with_io_state};
use crate::io::{DescriptorLease, IoToken, RegisteredDescriptor};
use crate::{Buffer, RentBuffer};
use borrowed::{BorrowedRead, BorrowedWrite};
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
/// There are exactly two API families, so a caller can tell from the signature
/// alone whether it keeps its buffer or hands it over. The driver level knows
/// only the rent family: every queued data-movement operation is a rent op
/// (`Op::ReadRent`/`Op::WriteRent`) carrying a pooled owned `Buffer`, on both
/// backends. The borrowed family ([`borrowed`]) is a thin adaptation over it.
///
/// | method | buffer | fast path (socket ready) | slow path (EAGAIN) |
/// |---|---|---|---|
/// | `read_rent`, `write_rent` | **rented**: the caller hands over an owned `T: RentBuffer` and gets the same allocation back | direct syscall against the caller's allocation (`read_rent` skips it on completion backends: the queued Recv doubles as readiness) | rent op, zero copies |
/// | `poll_read`, `poll_read_vectored`, `poll_write`, `poll_write_vectored`, `HyperRead::poll_read` | **borrowed**: the caller's slices | direct `read(2)`/`write(2)`/`readv(2)`/`writev(2)` straight from/into the slices — zero copies | rent op armed with a pooled `Buffer`; completion is copied into (or out of) the caller's slice — one copy, on the waiting path only |
/// | `write_all_static` | borrowed `&'static [u8]` | direct `write(2)` | `Op::WriteStatic` points the backend straight at the `'static` bytes — the `'static` lifetime is exactly why queueing without a copy is sound |
///
/// Writing a head followed by a body is an HTTP response detail, not a stream
/// capability, so `HttpConnection::send_response` does that itself.
///
/// [`borrowed`]: self::borrowed
#[derive(Debug)]
pub struct AsyncTcpStream {
    descriptor: Option<RegisteredDescriptor>,
    /// Borrowed-read adaptation: an armed rent read plus leftover bytes from a
    /// completed rent read that the completing poll's buffer could not take.
    read: BorrowedRead,
    /// Borrowed-write adaptation: an armed rent write holding a pooled copy of
    /// the caller's bytes while the socket was not writable.
    write: BorrowedWrite,
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
            read: BorrowedRead::new(),
            write: BorrowedWrite::new(),
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
        // Cancel armed rent ops before the descriptor identity leaves this
        // wrapper. Dropping the armed futures cancels and discards their
        // operations on the owning shard; the backend's operation lease keeps
        // the descriptor alive until terminal completion either way.
        self.read.clear();
        self.write.clear();
        self.descriptor
            .take()
            .expect("live stream must own its descriptor")
            .into_raw_fd()
            .expect("failed to transfer stream descriptor")
    }
}

impl Drop for AsyncTcpStream {
    fn drop(&mut self) {
        // Cancel armed rent ops first so cancellation is ordered before the
        // registered descriptor is released. Dropping an armed `IoFuture`
        // cancels and discards its operation on the owning shard; the
        // backend's operation lease keeps the fd identity alive until the
        // terminal completion either way.
        self.read.clear();
        self.write.clear();
    }
}

/// Borrowed-over-rent adaptation for the stream poll family.
///
/// This is monoio-compat's shape with a syscall-first fast path: the driver
/// level only ever sees rent operations (`Op::ReadRent`/`Op::WriteRent`
/// carrying pooled owned [`Buffer`]s), and the borrowed poll family arms one
/// of them whenever a call cannot complete inside its own `poll` invocation,
/// copying between the pooled buffer and the caller's slice at completion.
/// While the socket is ready the borrowed path stays a plain nonblocking
/// syscall straight from/into the caller's slice — zero copies, zero queued
/// operations, on both backends — which is what keeps the ready case from
/// gaining anything.
///
/// Each direction holds at most one armed operation. A re-poll before that
/// operation completes never re-arms and never cancels: it only refreshes the
/// waker through `IoFuture::poll`. An unrelated wake-up therefore cannot
/// submit a duplicate operation for the descriptor (the kernel would answer a
/// duplicate with `EEXIST`, so the mock-backend tests below stand in for it).
///
/// Architecture audit: the adaptation adds only stream-local state (one
/// armed `IoFuture` and one leftover `Buffer` per direction, both owned by the
/// creating shard). It adds no shared state, locks, atomics, runtime
/// creation, blocking calls, or cross-core traffic; the intermediate buffers
/// come from the existing thread-local pooled `Buffer`.
mod borrowed {
    use super::{is_wouldblock, try_read, try_read_vectored, try_write, try_write_vectored};
    use crate::cpu::CpuIoState;
    use crate::io::future::IoFuture;
    use crate::io::{CompletionKind, DescriptorLease, Op};
    use crate::Buffer;
    use std::future::Future;
    use std::io;
    use std::io::IoSliceMut;
    use std::os::unix::io::RawFd;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    fn unexpected_completion(other: CompletionKind, context: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Unexpected completion for {context}: {other:?}"),
        )
    }

    /// Copy a completed rent read into the caller's slice, stashing whatever
    /// the slice could not take for the next poll (the completing poll may
    /// pass a smaller buffer than the one that armed the read).
    fn deliver_rent_read(
        buffer: Buffer,
        dst: &mut [u8],
        leftover: &mut Option<Buffer>,
        consumed: &mut usize,
    ) -> usize {
        let filled = buffer.len();
        if filled == 0 {
            return 0; // EOF: never stash an empty leftover.
        }
        let copy_len = filled.min(dst.len());
        dst[..copy_len].copy_from_slice(&buffer[..copy_len]);
        if copy_len < filled {
            *leftover = Some(buffer);
            *consumed = copy_len;
        }
        copy_len
    }

    /// [`deliver_rent_read`] across the caller's slices, in order.
    fn deliver_rent_read_vectored(
        buffer: Buffer,
        dsts: &mut [IoSliceMut<'_>],
        leftover: &mut Option<Buffer>,
        consumed: &mut usize,
    ) -> usize {
        let filled = buffer.len();
        if filled == 0 {
            return 0;
        }
        let mut remaining = &buffer[..filled];
        let mut copied = 0;
        for dst in dsts.iter_mut() {
            if remaining.is_empty() {
                break;
            }
            let n = remaining.len().min(dst.len());
            dst[..n].copy_from_slice(&remaining[..n]);
            remaining = &remaining[n..];
            copied += n;
        }
        if copied < filled {
            *leftover = Some(buffer);
            *consumed = copied;
        }
        copied
    }

    /// Borrowed read side: armed rent read + leftover completed bytes.
    #[derive(Debug)]
    pub(super) struct BorrowedRead {
        /// Rent read armed by a previous `EAGAIN`, waiting for completion. The
        /// pooled buffer it reads into lives in the backend's operation slot
        /// and returns inside the completion.
        wait: Option<IoFuture>,
        /// Completed rent read bytes the caller has not consumed yet.
        leftover: Option<Buffer>,
        /// Consumed prefix of `leftover`.
        consumed: usize,
    }

    impl BorrowedRead {
        pub(super) fn new() -> Self {
            Self {
                wait: None,
                leftover: None,
                consumed: 0,
            }
        }

        /// Cancel an armed read and drop leftover bytes. Dropping the armed
        /// `IoFuture` cancels and discards the operation on the owning shard.
        pub(super) fn clear(&mut self) {
            self.wait = None;
            self.leftover = None;
            self.consumed = 0;
        }

        /// Poll the armed rent read, taking it out when it completed.
        fn poll_armed(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Buffer>> {
            let mut future = self
                .wait
                .take()
                .expect("armed read polled without an operation");
            match Pin::new(&mut future).poll(cx) {
                Poll::Pending => {
                    self.wait = Some(future);
                    Poll::Pending
                }
                Poll::Ready(Ok(CompletionKind::ReadRent { buffer })) => Poll::Ready(Ok(buffer)),
                Poll::Ready(Ok(other)) => {
                    Poll::Ready(Err(unexpected_completion(other, "borrowed read")))
                }
                Poll::Ready(Err(err)) => Poll::Ready(Err(err.into())),
            }
        }

        /// Hand out leftover bytes from a completed rent read. Returns `None`
        /// when there is nothing (or an empty amount) to serve so the caller
        /// falls through to the socket; `Ok(0)` must remain EOF-only.
        fn serve_leftover(&mut self, dst: &mut [u8]) -> Option<usize> {
            let (served, exhausted) = {
                let leftover = self.leftover.as_ref()?;
                let available = &leftover[self.consumed..];
                let n = available.len().min(dst.len());
                dst[..n].copy_from_slice(&available[..n]);
                (n, self.consumed + n == leftover.len())
            };
            if exhausted {
                self.leftover = None;
                self.consumed = 0;
            } else {
                self.consumed += served;
            }
            (served > 0).then_some(served)
        }

        /// Arm one `Op::ReadRent` of `capacity` bytes and poll it once. The
        /// common result is `Pending` (stored as the armed read); a backend
        /// that completed the operation synchronously returns its buffer.
        fn arm_read(
            &mut self,
            state: &CpuIoState,
            fd: RawFd,
            lease: DescriptorLease,
            cx: &mut Context<'_>,
            capacity: usize,
        ) -> Poll<io::Result<Buffer>> {
            let op = Op::ReadRent {
                fd,
                buffer: Buffer::new_uninitialized(capacity),
            };
            let token = match state.io_backend.submit_with_descriptor(op, lease) {
                Ok(token) => token,
                Err(error) => return Poll::Ready(Err(error.into())),
            };
            let mut future = IoFuture::new(token);
            match Pin::new(&mut future).poll(cx) {
                Poll::Pending => {
                    self.wait = Some(future);
                    Poll::Pending
                }
                Poll::Ready(Ok(CompletionKind::ReadRent { buffer })) => Poll::Ready(Ok(buffer)),
                Poll::Ready(Ok(other)) => {
                    Poll::Ready(Err(unexpected_completion(other, "borrowed read")))
                }
                Poll::Ready(Err(err)) => Poll::Ready(Err(err.into())),
            }
        }

        /// Borrowed `poll_read`: leftover, armed op, direct syscall, then a
        /// rented read armed with a pooled buffer. The syscall fast path writes
        /// into the caller's slice with zero copies; the rented slow path pays
        /// one copy out of the pooled buffer at completion.
        pub(super) fn poll_read_slice(
            &mut self,
            state: &CpuIoState,
            fd: RawFd,
            lease: DescriptorLease,
            cx: &mut Context<'_>,
            dst: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            if dst.is_empty() {
                return Poll::Ready(Ok(0));
            }
            if let Some(n) = self.serve_leftover(dst) {
                return Poll::Ready(Ok(n));
            }
            if self.wait.is_some() {
                match self.poll_armed(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(buffer)) => {
                        return Poll::Ready(Ok(deliver_rent_read(
                            buffer,
                            dst,
                            &mut self.leftover,
                            &mut self.consumed,
                        )));
                    }
                    // A completion-backend read op can only report EAGAIN for a
                    // non-socket descriptor; treat it like the fast path's
                    // EAGAIN and fall through to re-arm below.
                    Poll::Ready(Err(err)) if is_wouldblock(&err) => {}
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                }
            }

            match try_read(fd, dst) {
                Ok(bytes) => return Poll::Ready(Ok(bytes)),
                Err(err) if is_wouldblock(&err) => {}
                Err(err) => return Poll::Ready(Err(err)),
            }

            match self.arm_read(state, fd, lease, cx, dst.len()) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(buffer)) => Poll::Ready(Ok(deliver_rent_read(
                    buffer,
                    dst,
                    &mut self.leftover,
                    &mut self.consumed,
                ))),
                Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            }
        }

        /// Borrowed `poll_read_vectored` with more than one destination slice.
        /// The slow path arms a scalar rented read sized to the destinations'
        /// total and distributes the completion across them in order.
        pub(super) fn poll_read_vectored(
            &mut self,
            state: &CpuIoState,
            fd: RawFd,
            lease: DescriptorLease,
            cx: &mut Context<'_>,
            dsts: &mut [IoSliceMut<'_>],
        ) -> Poll<io::Result<usize>> {
            let total: usize = dsts.iter().map(|dst| dst.len()).sum();
            if total == 0 {
                return Poll::Ready(Ok(0));
            }
            if let Some(n) = self.serve_leftover_vectored(dsts) {
                return Poll::Ready(Ok(n));
            }
            if self.wait.is_some() {
                match self.poll_armed(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(buffer)) => {
                        return Poll::Ready(Ok(deliver_rent_read_vectored(
                            buffer,
                            dsts,
                            &mut self.leftover,
                            &mut self.consumed,
                        )));
                    }
                    Poll::Ready(Err(err)) if is_wouldblock(&err) => {}
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                }
            }

            match try_read_vectored(fd, dsts) {
                Ok(bytes) => return Poll::Ready(Ok(bytes)),
                Err(err) if is_wouldblock(&err) => {}
                Err(err) => return Poll::Ready(Err(err)),
            }

            match self.arm_read(state, fd, lease, cx, total) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(buffer)) => Poll::Ready(Ok(deliver_rent_read_vectored(
                    buffer,
                    dsts,
                    &mut self.leftover,
                    &mut self.consumed,
                ))),
                Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            }
        }

        /// [`serve_leftover`] across the caller's slices, in order.
        fn serve_leftover_vectored(&mut self, dsts: &mut [IoSliceMut<'_>]) -> Option<usize> {
            let (served, exhausted) = {
                let leftover = self.leftover.as_ref()?;
                let mut available = &leftover[self.consumed..];
                let mut served = 0;
                for dst in dsts.iter_mut() {
                    if available.is_empty() {
                        break;
                    }
                    let n = available.len().min(dst.len());
                    dst[..n].copy_from_slice(&available[..n]);
                    available = &available[n..];
                    served += n;
                }
                (served, self.consumed + served == leftover.len())
            };
            if exhausted {
                self.leftover = None;
                self.consumed = 0;
            } else {
                self.consumed += served;
            }
            (served > 0).then_some(served)
        }
    }

    /// Borrowed write side: armed rent write holding the caller's bytes.
    #[derive(Debug)]
    pub(super) struct BorrowedWrite {
        /// Rent write armed by a previous `EAGAIN`. Its pooled `Buffer` holds
        /// a copy of the caller's bytes while the socket is not writable.
        wait: Option<IoFuture>,
    }

    impl BorrowedWrite {
        pub(super) fn new() -> Self {
            Self { wait: None }
        }

        /// Cancel an armed write. Dropping the armed `IoFuture` cancels and
        /// discards the operation on the owning shard.
        pub(super) fn clear(&mut self) {
            self.wait = None;
        }

        /// Poll the armed rent write. On completion returns the byte count the
        /// operation wrote; the caller returns it as this poll's progress.
        fn poll_armed(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
            let mut future = self
                .wait
                .take()
                .expect("armed write polled without an operation");
            match Pin::new(&mut future).poll(cx) {
                Poll::Pending => {
                    self.wait = Some(future);
                    Poll::Pending
                }
                Poll::Ready(Ok(CompletionKind::WriteRent { bytes_written, .. })) => {
                    if bytes_written == 0 {
                        Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "borrowed write completed with zero bytes",
                        )))
                    } else {
                        Poll::Ready(Ok(bytes_written))
                    }
                }
                Poll::Ready(Ok(other)) => {
                    Poll::Ready(Err(unexpected_completion(other, "borrowed write")))
                }
                Poll::Ready(Err(err)) => Poll::Ready(Err(err.into())),
            }
        }

        /// Shared tail of the borrowed write paths: arm one `Op::WriteRent`
        /// carrying `data` and poll it once. `Pending` stores the armed write.
        fn arm_write(
            &mut self,
            state: &CpuIoState,
            fd: RawFd,
            lease: DescriptorLease,
            cx: &mut Context<'_>,
            data: Buffer,
        ) -> Poll<io::Result<usize>> {
            let op = Op::WriteRent { fd, data };
            let token = match state.io_backend.submit_with_descriptor(op, lease) {
                Ok(token) => token,
                Err(error) => return Poll::Ready(Err(error.into())),
            };
            let mut future = IoFuture::new(token);
            match Pin::new(&mut future).poll(cx) {
                Poll::Pending => {
                    self.wait = Some(future);
                    Poll::Pending
                }
                Poll::Ready(Ok(CompletionKind::WriteRent { bytes_written, .. })) => {
                    if bytes_written == 0 {
                        Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "borrowed write completed with zero bytes",
                        )))
                    } else {
                        Poll::Ready(Ok(bytes_written))
                    }
                }
                Poll::Ready(Ok(other)) => {
                    Poll::Ready(Err(unexpected_completion(other, "borrowed write")))
                }
                Poll::Ready(Err(err)) => Poll::Ready(Err(err.into())),
            }
        }

        /// Borrowed `poll_write`: armed op, direct `write(2)` from the
        /// caller's slice, then a rented write of a pooled copy. The fast path
        /// is byte-for-byte today's zero-copy write; only the EAGAIN path
        /// copies, into the pooled `Buffer` the rent op owns.
        pub(super) fn poll_write_slice(
            &mut self,
            state: &CpuIoState,
            fd: RawFd,
            lease: DescriptorLease,
            cx: &mut Context<'_>,
            src: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.wait.is_some() {
                match self.poll_armed(cx) {
                    Poll::Pending => return Poll::Pending,
                    // An armed write cannot legitimately complete EAGAIN on a
                    // socket; fall through to the fast path and re-arm.
                    Poll::Ready(Err(err)) if is_wouldblock(&err) => {}
                    Poll::Ready(result) => return Poll::Ready(result),
                }
            }

            match try_write(fd, src) {
                Ok(bytes) => return Poll::Ready(Ok(bytes)),
                Err(err) if is_wouldblock(&err) => {}
                Err(err) => return Poll::Ready(Err(err)),
            }

            self.arm_write(state, fd, lease, cx, Buffer::from_slice(src))
        }

        /// Borrowed `poll_write_vectored` with more than one source slice. The
        /// slow path concatenates the slices into one pooled buffer so the
        /// rented write carries a single contiguous payload.
        pub(super) fn poll_write_vectored(
            &mut self,
            state: &CpuIoState,
            fd: RawFd,
            lease: DescriptorLease,
            cx: &mut Context<'_>,
            srcs: &[std::io::IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            if self.wait.is_some() {
                match self.poll_armed(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(err)) if is_wouldblock(&err) => {}
                    Poll::Ready(result) => return Poll::Ready(result),
                }
            }

            match try_write_vectored(fd, srcs) {
                Ok(bytes) => return Poll::Ready(Ok(bytes)),
                Err(err) if is_wouldblock(&err) => {}
                Err(err) => return Poll::Ready(Err(err)),
            }

            let total: usize = srcs.iter().map(|src| src.len()).sum();
            let mut contiguous = Vec::with_capacity(total);
            for src in srcs {
                contiguous.extend_from_slice(src);
            }
            self.arm_write(state, fd, lease, cx, Buffer::from(contiguous))
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
        with_io_state(|state| {
            // SAFETY: AsyncTcpStream is Unpin in practice because it contains no
            // self-referential fields; poll never moves fields after projection.
            let this = self.as_mut().get_mut();
            let fd = this.as_raw_fd();
            let descriptor = this.descriptor_lease();
            this.read.poll_read_slice(state, fd, descriptor, cx, buf)
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
            this.read
                .poll_read_vectored(state, fd, descriptor, cx, bufs)
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
            this.write.poll_write_slice(state, fd, descriptor, cx, buf)
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
            this.write
                .poll_write_vectored(state, fd, descriptor, cx, bufs)
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

            match this
                .read
                .poll_read_slice(state, fd, descriptor, cx, read_buf)
            {
                Poll::Ready(Ok(bytes_read)) => {
                    // SAFETY: the borrowed read initialized exactly `bytes_read`
                    // bytes of the cursor's tail.
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
mod borrowed_repoll_tests {
    use super::borrowed::BorrowedRead;
    use crate::cpu::{test_io_state, RuntimeContextGuard};
    use crate::io::{
        CompletionKind, DescriptorLease, IoError, IoProvider, IoToken, Op, RegisteredDescriptor,
    };
    use crate::timer::TimerWheel;
    use std::cell::RefCell;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::rc::Rc;
    use std::task::{Context, Poll};

    #[derive(Default)]
    struct BackendState {
        pending: Vec<PendingRent>,
        cancellations: usize,
        submissions: usize,
    }

    struct PendingRent {
        token: IoToken,
        fd: i32,
        _descriptor: DescriptorLease,
    }

    /// Rent-op stand-in for the kernel's EEXIST answer: a second armed
    /// operation on the same descriptor is rejected outright, so a duplicate
    /// submission can never hide inside this test.
    struct DuplicateRentBackend(Rc<RefCell<BackendState>>);

    impl DuplicateRentBackend {
        fn admit(&self, fd: i32, descriptor: DescriptorLease) -> Result<IoToken, IoError> {
            let mut state = self.0.borrow_mut();
            if state.pending.iter().any(|pending| pending.fd == fd) {
                return Err(IoError::Io(std::io::Error::from_raw_os_error(libc::EEXIST)));
            }
            let token = IoToken::new();
            state.submissions += 1;
            state.pending.push(PendingRent {
                token,
                fd,
                _descriptor: descriptor,
            });
            Ok(token)
        }
    }

    impl IoProvider for DuplicateRentBackend {
        type Completion = (IoToken, Result<CompletionKind, IoError>);

        fn submit(&self, op: Op) -> Result<IoToken, IoError> {
            let Op::ReadRent { fd, buffer } = op else {
                return Err(IoError::Other("rent read expected".into()));
            };
            drop(buffer);
            // SAFETY: the test submitted this descriptor on its own shard; the
            // duplicate-lease acquisition models the backend's operation lease.
            let lease = DescriptorLease::acquire(fd).map_err(IoError::Io)?;
            self.admit(fd, lease)
        }

        fn submit_with_descriptor(
            &self,
            op: Op,
            descriptor: DescriptorLease,
        ) -> Result<IoToken, IoError> {
            let Op::ReadRent { fd, .. } = op else {
                return Err(IoError::Other("rent read expected".into()));
            };
            self.admit(fd, descriptor)
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

    /// The borrowed-read contract that replaced readiness registration: a
    /// re-poll woken by something unrelated must neither submit a duplicate
    /// rent op for the descriptor (the kernel would answer `EEXIST`) nor
    /// cancel the armed one — it only refreshes the waker. The armed op then
    /// delivers its completion to whichever poll comes next.
    #[test]
    fn repoll_after_unrelated_wake_never_rearms_the_rent_read() {
        let state = Rc::new(RefCell::new(BackendState::default()));
        let io_state = test_io_state(DuplicateRentBackend(state.clone()));
        let mut fds = [0; 2];
        // SAFETY: `fds` contains two writable slots for `pipe` to fill.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: the successful pipe call transferred each endpoint to this test.
        let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // SAFETY: same as above for the other endpoint.
        let _write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        // The syscall fast path needs a nonblocking descriptor; a blocking
        // pipe would stall the test thread instead of returning EAGAIN.
        // SAFETY: `read_end` is a live pipe endpoint owned by this test.
        unsafe {
            let flags = libc::fcntl(read_end.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(
                read_end.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            );
        }
        let descriptor = RegisteredDescriptor::acquire(read_end.as_raw_fd()).expect("lease fd");
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut timers = TimerWheel::default();
        // SAFETY: the test keeps the state and timer on this owner thread
        // until the guard is dropped.
        let _guard = unsafe { RuntimeContextGuard::enter(&io_state, &mut timers, Some(0)) };
        let mut borrowed = BorrowedRead::new();
        let mut slot = [0_u8; 64];
        let lease = || descriptor.lease();

        // First poll: syscall hits EAGAIN on the empty pipe, so the read arms
        // exactly one rent op and goes Pending.
        let poll =
            borrowed.poll_read_slice(&io_state, read_end.as_raw_fd(), lease(), &mut cx, &mut slot);
        assert!(matches!(poll, Poll::Pending));
        assert_eq!(state.borrow().submissions, 1);
        assert_eq!(io_state.io_backend.pending_operation_count(), 1);
        assert_eq!(io_state.io_backend.retained_descriptor_count(), 1);

        // Model the task being polled again by an unrelated wake before the
        // armed read completes: no re-arm, no cancel, same single pending op.
        let repoll =
            borrowed.poll_read_slice(&io_state, read_end.as_raw_fd(), lease(), &mut cx, &mut slot);
        assert!(
            matches!(repoll, Poll::Pending),
            "re-poll should keep waiting on the armed read, not re-arm: {repoll:?}"
        );
        assert_eq!(state.borrow().submissions, 1, "duplicate rent submission");
        assert_eq!(state.borrow().cancellations, 0);
        assert_eq!(io_state.io_backend.pending_operation_count(), 1);

        // Complete the armed op with data, then deliver it to the next poll.
        // Consuming the completion retires the backend-side operation slot, as
        // the CQE drain does on a real backend.
        let token = state.borrow_mut().pending.remove(0).token;
        // SAFETY: the test owns the shard-local state and this token.
        unsafe {
            if let Some(waker) = io_state.insert_completed_and_take_waker(
                token,
                Ok(CompletionKind::ReadRent {
                    buffer: crate::Buffer::from_slice(b"data"),
                }),
            ) {
                drop(waker);
            }
        }
        let delivered =
            borrowed.poll_read_slice(&io_state, read_end.as_raw_fd(), lease(), &mut cx, &mut slot);
        assert!(matches!(delivered, Poll::Ready(Ok(4))), "{delivered:?}");
        assert_eq!(&slot[..4], b"data");
        assert_eq!(state.borrow().cancellations, 0);
        assert_eq!(io_state.io_backend.pending_operation_count(), 0);
        assert_eq!(io_state.io_backend.retained_descriptor_count(), 0);
    }
}
