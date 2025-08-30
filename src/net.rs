//! Async networking primitives for miniss.

use crate::cpu::io_state;
use crate::io::{future::IoFuture, CompletionKind, Op};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};

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
        let op = Op::Accept {
            fd: self.inner.as_raw_fd(),
        };
        let token = state.io_backend.submit(op);
        let future = IoFuture::new(token);

        match future.await {
            Ok(CompletionKind::Accept { fd, addr }) => {
                let stream = unsafe { TcpStream::from_raw_fd(fd) };
                Ok((AsyncTcpStream { inner: stream }, addr))
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion kind",
            )),
            Err(e) => Err(e.into()),
        }
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
}

impl AsyncTcpStream {
    /// Reads some bytes from the stream.
    /// Returns the number of bytes read and a buffer containing the data.
    pub async fn read(&self) -> io::Result<(usize, crate::buffer::Buffer)> {
        let state = io_state();
        // Get a buffer from the pool to read into.
        let buffer = crate::buffer::BufferPool::get(crate::buffer::BUFFER_SIZE); // BUFFER_SIZE is now public
        let op = Op::Read {
            fd: self.inner.as_raw_fd(),
            offset: 0,
            len: buffer.capacity(),
        };
        let token = state.io_backend.submit(op);
        let future = IoFuture::new(token);

        match future.await {
            Ok(CompletionKind::Read { bytes_read, data }) => Ok((bytes_read, data)),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion kind",
            )),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes a buffer into this writer, returning how many bytes were written.
    pub async fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let state = io_state();
        let mut buffer = crate::buffer::BufferPool::get(buf.len());
        buffer.copy_from_slice(buf);
        let op = Op::Write {
            fd: self.inner.as_raw_fd(),
            offset: 0,
            data: buffer,
        };
        let token = state.io_backend.submit(op);
        let future = IoFuture::new(token);

        match future.await {
            Ok(CompletionKind::Write { bytes_written }) => Ok(bytes_written),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion kind",
            )),
            Err(e) => Err(e.into()),
        }
    }

    /// Attempts to write an entire buffer into this writer.
    pub async fn write_all(&self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            match self.write(buf).await {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    ));
                }
                Ok(n) => buf = &buf[n..],
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl From<TcpStream> for AsyncTcpStream {
    fn from(stream: TcpStream) -> Self {
        stream.set_nonblocking(true).ok();
        Self { inner: stream }
    }
}

impl AsRawFd for AsyncTcpStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl IntoRawFd for AsyncTcpStream {
    fn into_raw_fd(self) -> RawFd {
        self.inner.into_raw_fd()
    }
}

/// An asynchronous UDP socket.
///
/// This struct represents a UDP socket that supports asynchronous I/O operations.
/// It wraps a standard `std::net::UdpSocket` and provides non-blocking operations
/// for use with the async runtime's I/O backends.
///
/// # Platform Support
///
/// - On Linux with kernel 5.10+: Uses io_uring for zero-copy operations
/// - On Linux with older kernels: Uses epoll for event-driven I/O
/// - On macOS: Uses kqueue for BSD-style event notification
///
/// # Examples
///
/// ```
/// use rust_miniss::{net::AsyncUdpSocket, Runtime};
/// use std::net::SocketAddr;
///
/// let runtime = Runtime::new();
/// runtime.block_on(async {
///     // Bind to a local address
///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
///     let socket = AsyncUdpSocket::bind(addr).expect("Failed to bind socket");
///     
///     // Send data to a remote address
///     let remote_addr: SocketAddr = "127.0.0.1:9090".parse().unwrap();
///     let data = b"Hello, UDP!";
///     let bytes_sent = socket.send_to(data, remote_addr).await.expect("Failed to send");
///     
///     // Receive data from any address
///     let mut buf = [0; 1024];
///     let (bytes_received, src_addr) = socket.recv_from(&mut buf).await.expect("Failed to receive");
/// });
/// ```
#[derive(Debug)]
pub struct AsyncUdpSocket {
    inner: UdpSocket,
}

impl AsyncUdpSocket {
    /// Creates a UDP socket bound to the specified address.
    ///
    /// This function creates a new UDP socket and binds it to the specified address.
    ///
    /// # Arguments
    ///
    /// * `addr` - The address to bind the socket to
    ///
    /// # Returns
    ///
    /// * `Ok(AsyncUdpSocket)` - Successfully bound socket
    /// * `Err(io::Error)` - Failed to create or bind socket
    ///
    /// # Examples
    ///
    /// ```
    /// use rust_miniss::net::AsyncUdpSocket;
    /// use std::net::SocketAddr;
    ///
    /// let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    /// let socket = AsyncUdpSocket::bind(addr).expect("Failed to bind socket");
    /// ```
    pub fn bind<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr.into())?;
        socket.set_nonblocking(true)?;
        Ok(Self { inner: socket })
    }

    /// Sends data to the specified address.
    ///
    /// This function performs an asynchronous send operation to the specified address.
    ///
    /// # Arguments
    ///
    /// * `buf` - The data to send
    /// * `target` - The destination address
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - Number of bytes sent
    /// * `Err(io::Error)` - Failed to send data
    ///
    /// # Examples
    ///
    /// ```
    /// use rust_miniss::{net::AsyncUdpSocket, Runtime};
    /// use std::net::SocketAddr;
    ///
    /// let runtime = Runtime::new();
    /// runtime.block_on(async {
    ///     let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    ///     let socket = AsyncUdpSocket::bind(addr).expect("Failed to bind socket");
    ///     
    ///     let remote_addr: SocketAddr = "127.0.0.1:9090".parse().unwrap();
    ///     let data = b"Hello, UDP!";
    ///     let bytes_sent = socket.send_to(data, remote_addr).await.expect("Failed to send");
    /// });
    /// ```
    pub async fn send_to<A: Into<SocketAddr>>(&self, buf: &[u8], target: A) -> io::Result<usize> {
        let state = io_state();
        let mut buffer = crate::buffer::BufferPool::get(buf.len());
        buffer.copy_from_slice(buf);

        let op = Op::UdpSend {
            fd: self.inner.as_raw_fd(),
            data: buffer,
            addr: target.into(),
        };
        let token = state.io_backend.submit(op);
        let future = IoFuture::new(token);

        match future.await {
            Ok(CompletionKind::UdpSend {
                bytes_written,
                data,
            }) => {
                data.recycle(); // Recycle the buffer after send completes
                Ok(bytes_written)
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion kind for UdpSend",
            )),
            Err(e) => Err(e.into()),
        }
    }

    /// Receives data from any address.
    ///
    /// This function performs an asynchronous receive operation, waiting for data
    /// from any address.
    ///
    /// # Arguments
    ///
    /// * `buf` - Buffer to store received data
    ///
    /// # Returns
    ///
    /// * `Ok((usize, SocketAddr))` - Tuple of (bytes_received, source_address)
    /// * `Err(io::Error)` - Failed to receive data
    ///
    /// # Examples
    ///
    /// ```
    /// use rust_miniss::{net::AsyncUdpSocket, Runtime};
    /// use std::net::SocketAddr;
    ///
    /// let runtime = Runtime::new();
    /// runtime.block_on(async {
    ///     let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    ///     let socket = AsyncUdpSocket::bind(addr).expect("Failed to bind socket");
    ///     
    ///     let mut buf = [0; 1024];
    ///     let (bytes_received, src_addr) = socket.recv_from(&mut buf).await.expect("Failed to receive");
    /// });
    /// ```
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let state = io_state();
        // Get a buffer from the pool to receive into.
        let buffer_for_recv = crate::buffer::BufferPool::get(buf.len());

        let op = Op::UdpRecv {
            fd: self.inner.as_raw_fd(),
            buffer: buffer_for_recv,
        };
        let token = state.io_backend.submit(op);
        let future = IoFuture::new(token);

        match future.await {
            Ok(CompletionKind::UdpRecv {
                bytes_read,
                buffer,
                addr,
            }) => {
                // Copy received data to the caller's buffer
                let bytes_to_copy = std::cmp::min(bytes_read, buf.len());
                buf[..bytes_to_copy].copy_from_slice(&buffer[..bytes_to_copy]);
                buffer.recycle(); // Recycle the buffer after use
                Ok((bytes_read, addr))
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion kind for UdpRecv",
            )),
            Err(e) => Err(e.into()),
        }
    }

    /// Returns the socket address that this socket was bound to.
    ///
    /// # Returns
    ///
    /// * `Ok(SocketAddr)` - The local socket address
    /// * `Err(io::Error)` - Failed to get local address
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

impl AsRawFd for AsyncUdpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl IntoRawFd for AsyncUdpSocket {
    fn into_raw_fd(self) -> RawFd {
        self.inner.into_raw_fd()
    }
}
