//! Async file I/O for miniss.
//!
//! This module provides high-level asynchronous file operations built on top
//! of the low-level I/O backends. It supports all three I/O backends:
//! - io_uring on Linux (high-performance zero-copy)
//! - epoll on Linux (event-driven)
//! - kqueue on macOS (BSD-style event notification)
//!
//! The implementation ensures memory safety and proper lifecycle management
//! for asynchronous operations across all backends.

use crate::cpu::io_state;
use crate::io::{future::IoFuture, CompletionKind, Op};
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use std::io;
use std::os::unix::io::{AsRawFd, IntoRawFd, RawFd};
use std::path::Path;

/// An asynchronous file handle.
///
/// This struct represents an open file that supports asynchronous I/O operations.
/// It wraps a standard `std::fs::File` and adds non-blocking mode for use with
/// the async runtime's I/O backends.
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
/// use rust_miniss::{fs::AsyncFile, Runtime};
///
/// let runtime = Runtime::new();
/// runtime.block_on(async {
///     // Create a new file
///     let file = AsyncFile::create("/tmp/test.txt").expect("Failed to create file");
///     
///     // Write data asynchronously
///     let data = b"Hello, async file I/O!";
///     let bytes_written = file.write_at(0, data).await.expect("Failed to write");
///     
///     // Read data asynchronously
///     let (bytes_read, content) = file.read_at(0, 1024).await.expect("Failed to read");
///     
///     // Sync to disk
///     file.sync_all().await.expect("Failed to sync");
/// });
/// ```
#[derive(Debug)]
pub struct AsyncFile {
    inner: std::fs::File,
}

impl AsyncFile {
    /// Opens an existing file for asynchronous operations.
    ///
    /// This function opens a file in read-only mode and sets it to non-blocking mode
    /// so it can be used with the async runtime's I/O backends.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file to open
    ///
    /// # Returns
    ///
    /// * `Ok(AsyncFile)` - Successfully opened file handle
    /// * `Err(io::Error)` - Failed to open file
    ///
    /// # Examples
    ///
    /// ```
    /// use rust_miniss::fs::AsyncFile;
    ///
    /// let file = AsyncFile::open("/etc/passwd").expect("Failed to open /etc/passwd");
    /// ```
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        // Set non-blocking mode
        let fd = file.as_raw_fd();
        let flags = fcntl(fd, FcntlArg::F_GETFL).map_err(io::Error::other)?;
        let flags = OFlag::from_bits_truncate(flags);
        let new_flags = flags | OFlag::O_NONBLOCK;
        fcntl(fd, FcntlArg::F_SETFL(new_flags)).map_err(io::Error::other)?;
        Ok(Self { inner: file })
    }

    /// Creates a new file for asynchronous operations.
    ///
    /// This function creates a new file (or truncates an existing one) and sets it
    /// to non-blocking mode for use with the async runtime.
    ///
    /// # Arguments
    ///
    /// * `path` - Path where the new file should be created
    ///
    /// # Returns
    ///
    /// * `Ok(AsyncFile)` - Successfully created file handle
    /// * `Err(io::Error)` - Failed to create file
    ///
    /// # Examples
    ///
    /// ```
    /// use rust_miniss::fs::AsyncFile;
    ///
    /// let file = AsyncFile::create("/tmp/new_file.txt").expect("Failed to create file");
    /// ```
    pub fn create<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = std::fs::File::create(path)?;
        // Set non-blocking mode
        let fd = file.as_raw_fd();
        let flags = fcntl(fd, FcntlArg::F_GETFL).map_err(io::Error::other)?;
        let flags = OFlag::from_bits_truncate(flags);
        let new_flags = flags | OFlag::O_NONBLOCK;
        fcntl(fd, FcntlArg::F_SETFL(new_flags)).map_err(io::Error::other)?;
        Ok(Self { inner: file })
    }

    /// Reads data from the file at the specified offset.
    ///
    /// This function performs an asynchronous read operation starting at the given
    /// offset. It returns both the number of bytes actually read and the data itself.
    ///
    /// # Arguments
    ///
    /// * `offset` - Byte offset from the beginning of the file where reading should start
    /// * `len` - Maximum number of bytes to read
    ///
    /// # Returns
    ///
    /// * `Ok((usize, crate::buffer::Buffer))` - Tuple of (bytes_read, data)
    /// * `Err(io::Error)` - Failed to read from file
    ///
    /// # Examples
    ///
    /// ```
    /// use rust_miniss::{fs::AsyncFile, Runtime};
    ///
    /// let runtime = Runtime::new();
    /// runtime.block_on(async {
    ///     let file = AsyncFile::open("/etc/passwd").expect("Failed to open file");
    ///     let (bytes_read, data) = file.read_at(0, 1024).await.expect("Failed to read");
    ///     println!("Read {} bytes", bytes_read);
    /// });
    /// ```
    pub async fn read_at(
        &self,
        offset: u64,
        len: usize,
    ) -> io::Result<(usize, crate::buffer::Buffer)> {
        let state = io_state();
        let op = Op::ReadFile {
            fd: self.inner.as_raw_fd(),
            offset,
            len,
        };
        let token = state.io_backend.submit(op);
        let future = IoFuture::new(token);

        match future.await {
            Ok(CompletionKind::ReadFile { bytes_read, data }) => Ok((bytes_read, data)),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion kind",
            )),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes data to the file at the specified offset.
    ///
    /// This function performs an asynchronous write operation starting at the given
    /// offset. It returns the number of bytes actually written.
    ///
    /// # Arguments
    ///
    /// * `offset` - Byte offset from the beginning of the file where writing should start
    /// * `buf` - Data to write to the file
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - Number of bytes actually written
    /// * `Err(io::Error)` - Failed to write to file
    ///
    /// # Examples
    ///
    /// ```
    /// use rust_miniss::{fs::AsyncFile, Runtime};
    ///
    /// let runtime = Runtime::new();
    /// runtime.block_on(async {
    ///     let file = AsyncFile::create("/tmp/test.txt").expect("Failed to create file");
    ///     let data = b"Hello, world!";
    ///     let bytes_written = file.write_at(0, data).await.expect("Failed to write");
    ///     println!("Wrote {} bytes", bytes_written);
    /// });
    /// ```
    pub async fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        let state = io_state();
        let mut buffer = crate::buffer::BufferPool::get(buf.len());
        buffer.copy_from_slice(buf);

        let op = Op::WriteFile {
            fd: self.inner.as_raw_fd(),
            offset,
            data: buffer,
        };
        let token = state.io_backend.submit(op);
        let future = IoFuture::new(token);

        match future.await {
            Ok(CompletionKind::WriteFile { bytes_written }) => Ok(bytes_written),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion kind",
            )),
            Err(e) => Err(e.into()),
        }
    }

    /// Synchronizes the file's in-core state with storage.
    ///
    /// This function ensures that all previously written data is flushed to disk.
    /// It's an asynchronous version of `fsync(2)`.
    ///
    /// # Returns
    ///
    /// * `Ok(())` - Successfully synchronized
    /// * `Err(io::Error)` - Failed to synchronize
    ///
    /// # Examples
    ///
    /// ```
    /// use rust_miniss::{fs::AsyncFile, Runtime};
    ///
    /// let runtime = Runtime::new();
    /// runtime.block_on(async {
    ///     let file = AsyncFile::create("/tmp/test.txt").expect("Failed to create file");
    ///     let data = b"Important data";
    ///     file.write_at(0, data).await.expect("Failed to write");
    ///     file.sync_all().await.expect("Failed to sync");
    ///     println!("Data safely written to disk");
    /// });
    /// ```
    pub async fn sync_all(&self) -> io::Result<()> {
        let state = io_state();
        let op = Op::Fsync {
            fd: self.inner.as_raw_fd(),
        };
        let token = state.io_backend.submit(op);
        let future = IoFuture::new(token);

        match future.await {
            Ok(CompletionKind::Fsync) => Ok(()),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unexpected completion kind",
            )),
            Err(e) => Err(e.into()),
        }
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
