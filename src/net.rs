//! Async networking primitives for miniss.

use crate::cpu::io_state;
use crate::io::{future::IoFuture, CompletionKind, Op};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
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
        let op = Op::Accept { fd: self.inner.as_raw_fd() };
        let token = state.io_backend.submit(op);
        let future = IoFuture::new(token);

        match future.await {
            Ok(CompletionKind::Accept { fd, addr }) => {
                let stream = unsafe { TcpStream::from_raw_fd(fd) };
                Ok((AsyncTcpStream { inner: stream }, addr))
            }
            Ok(_) => Err(io::Error::new(io::ErrorKind::InvalidData, "Unexpected completion kind")),
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
    pub async fn read(&self) -> io::Result<(usize, Vec<u8>)> {
        let state = io_state();
        // Read up to a reasonable default size.
        let op = Op::Read { fd: self.inner.as_raw_fd(), offset: 0, len: 4096 };
        let token = state.io_backend.submit(op);
        let future = IoFuture::new(token);

        match future.await {
            Ok(CompletionKind::Read { bytes_read, data }) => Ok((bytes_read, data)),
            Ok(_) => Err(io::Error::new(io::ErrorKind::InvalidData, "Unexpected completion kind")),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes a buffer into this writer, returning how many bytes were written.
    pub async fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let state = io_state();
        let op = Op::Write { fd: self.inner.as_raw_fd(), offset: 0, data: buf.to_vec() };
        let token = state.io_backend.submit(op);
        let future = IoFuture::new(token);

        match future.await {
            Ok(CompletionKind::Write { bytes_written }) => Ok(bytes_written),
            Ok(_) => Err(io::Error::new(io::ErrorKind::InvalidData, "Unexpected completion kind")),
            Err(e) => Err(e.into()),
        }
    }

    /// Attempts to write an entire buffer into this writer.
    pub async fn write_all(&self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            match self.write(buf).await {
                Ok(0) => {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write whole buffer"));
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
