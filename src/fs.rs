//! Async file I/O for miniss.
//!
//! High-level file operations over the runtime's I/O backends: `io_uring` on
//! Linux, and the `mio` readiness backend (`epoll` on Linux, `kqueue` on macOS)
//! elsewhere. See `crate::io` for the per-backend lifecycle rules.
//!
//! ## Copy cost
//!
//! No file path is zero-copy: there is no `O_DIRECT`, no registered buffers and
//! no fixed-file table. Reads cost one kernel copy into a pooled `Buffer`;
//! writes cost one copy from the caller's slice into a `Buffer` plus one
//! kernel copy out. The `mio` backend additionally copies each payload a
//! second time when it hands the write to its thread pool.

use crate::cpu::with_io_state;
use crate::io::{future::IoFuture, CompletionKind, Op};
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use std::io;
use std::os::unix::io::{AsRawFd, IntoRawFd, RawFd};
use std::path::Path;

/// An asynchronous file handle.
///
/// # Platform Support
///
/// - Linux with an io_uring-capable kernel: io_uring (asynchronous, one kernel
///   copy per operation — not zero-copy).
/// - Linux and macOS otherwise: the `mio` readiness backend, which offloads
///   reads, writes, `sync_all` and `sync_data` to its thread pool. Flushes of
///   one file requested in the same shard round share a single syscall (see
///   `crate::io::mio_poll`).
#[derive(Debug)]
pub struct AsyncFile {
    inner: std::fs::File,
}

impl AsyncFile {
    /// Opens an existing file in read-only mode.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        Self::set_nonblocking(&file)?;
        Ok(Self { inner: file })
    }

    /// Creates (or truncates) a file.
    pub fn create<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = std::fs::File::create(path)?;
        Self::set_nonblocking(&file)?;
        Ok(Self { inner: file })
    }

    fn set_nonblocking(file: &std::fs::File) -> io::Result<()> {
        let fd = file.as_raw_fd();
        let flags = fcntl(fd, FcntlArg::F_GETFL).map_err(io::Error::other)?;
        let flags = OFlag::from_bits_truncate(flags);
        fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map_err(io::Error::other)?;
        Ok(())
    }

    /// Submits `op` and maps the terminal completion through `expect`.
    async fn run<T>(
        &self,
        op: Op,
        expect: impl FnOnce(CompletionKind) -> Option<T>,
    ) -> io::Result<T> {
        let token = with_io_state(|state| state.io_backend.submit(op));
        let completion = IoFuture::new(token).await.map_err(io::Error::from)?;
        expect(completion)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Unexpected completion kind"))
    }

    /// Reads at most `len` bytes starting at `offset`.
    pub async fn read_at(
        &self,
        offset: u64,
        len: usize,
    ) -> io::Result<(usize, crate::buffer::Buffer)> {
        let op = Op::ReadFile {
            fd: self.inner.as_raw_fd(),
            offset,
            len,
        };
        self.run(op, |completion| match completion {
            CompletionKind::ReadFile { bytes_read, data } => Some((bytes_read, data)),
            _ => None,
        })
        .await
    }

    /// Writes `data` at `offset`, returning the bytes written and the caller's
    /// buffer.
    ///
    /// The file-side mirror of `AsyncTcpStream::write_rent` and
    /// `AsyncUdpSocket::send_to_rent`: the caller hands over an owned buffer
    /// ([`crate::buffer::RentBuffer`] — `Buffer`, `Vec<u8>` or `Box<[u8]>`), the
    /// runtime owns it until the kernel is done, and the same allocation comes
    /// back. On the io_uring backend that is the whole story; the readiness
    /// backend writes from its thread pool, which needs an owned payload of its
    /// own, so there the returned buffer is a copy rather than the allocation
    /// the caller passed in.
    pub async fn write_rent_at<T>(&self, offset: u64, data: T) -> io::Result<(usize, T)>
    where
        T: crate::buffer::RentBuffer,
    {
        let op = Op::WriteFile {
            fd: self.inner.as_raw_fd(),
            offset,
            data: data.into(),
        };
        self.run(op, |completion| match completion {
            CompletionKind::WriteFile {
                bytes_written,
                data,
            } => Some((bytes_written, T::from(data))),
            _ => None,
        })
        .await
    }

    /// Writes `buf` at `offset`, returning the bytes actually written.
    ///
    /// The borrowed shape: `buf` is copied into a pooled buffer, which is what
    /// the queueing needs. Use [`AsyncFile::write_rent_at`] to keep the
    /// allocation.
    pub async fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        let (bytes_written, _) = self
            .write_rent_at(offset, crate::buffer::Buffer::from_slice(buf))
            .await?;
        Ok(bytes_written)
    }

    /// Asynchronous `fsync(2)`: data plus metadata.
    pub async fn sync_all(&self) -> io::Result<()> {
        let op = Op::Fsync {
            fd: self.inner.as_raw_fd(),
        };
        self.run(op, |completion| match completion {
            CompletionKind::Fsync => Some(()),
            _ => None,
        })
        .await
    }

    /// Asynchronous `fdatasync(2)`: data plus the metadata needed to read it
    /// back (size, extents), but not `mtime` and friends.
    ///
    /// Prefer this over [`AsyncFile::sync_all`] when modification time need not
    /// be durable: it is strictly less work for the storage stack. On the
    /// readiness backend it shares one group-commit round with concurrent
    /// flushes of the same file, and a round containing an `fsync` caller
    /// performs the stronger flush for everyone.
    pub async fn sync_data(&self) -> io::Result<()> {
        let op = Op::Fdatasync {
            fd: self.inner.as_raw_fd(),
        };
        self.run(op, |completion| match completion {
            CompletionKind::Fdatasync => Some(()),
            _ => None,
        })
        .await
    }
}

impl AsRawFd for AsyncFile {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl IntoRawFd for AsyncFile {
    fn into_raw_fd(self) -> RawFd {
        self.inner.into_raw_fd()
    }
}
