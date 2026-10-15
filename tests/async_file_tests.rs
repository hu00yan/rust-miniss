//! Tests for async file I/O functionality.
//!
//! These tests verify that the async file I/O implementation works correctly
//! across all supported I/O backends (io_uring, epoll, kqueue).
//!
//! ## Key Implementation Details
//!
//! ### Memory Safety for Async File Operations
//!
//! One critical aspect of our implementation is ensuring memory safety for
//! asynchronous file operations, particularly with io_uring. The issue we
//! encountered and fixed was:
//!
//! **Problem**: Async file write operations would return successfully but
//! write incorrect data to the file. Debugging showed that:
//! - The operation reported writing 4 bytes (correct)
//! - But the file contained garbage data instead of the expected "Hello, async file I/O!"
//!
//! **Root Cause**: In the io_uring backend, we were using `data.as_ptr()` where
//! `data` was borrowed via `ref data` in pattern matching. When the function
//! returned, this borrowed reference could become invalid, causing io_uring
//! to write from freed memory.
//!
//! **Solution**: Clone the data first to ensure it stays alive throughout the
//! entire I/O operation, then use the cloned data's pointer. The cloned data is
//! stored in the PendingOp to keep it alive until the operation completes.
//!
//! ```rust
//! // Fixed implementation in src/io/uring.rs
//! Op::WriteFile { fd, offset, ref data } => {
//!     // Clone data first to ensure it stays alive
//!     let cloned_data = data.clone();
//!     let entry = opcode::Write::new(types::Fd(fd), cloned_data.as_ptr(), cloned_data.len() as u32)
//!         .offset(offset)
//!         .build()
//!         .user_data(user_data);
//!     // Store the cloned data to keep it alive until completion
//!     (entry, PendingOp::WriteFile { op: Op::WriteFile { fd, offset, data: data.clone() }, data: cloned_data })
//! }
//! ```
//!
//! This fix ensures that file I/O operations work correctly across all backends.

use rust_miniss::{fs::AsyncFile, Runtime};
use std::io::Write;
use std::sync::Arc;
use tempfile::NamedTempFile;

#[test]
fn test_async_file_read_write() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        // Create a temporary file
        let mut temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let test_data = b"Hello, async file I/O!";
        temp_file
            .write_all(test_data)
            .expect("Failed to write to temp file");
        let temp_path = temp_file.path().to_path_buf();

        // Open the file asynchronously
        let async_file = AsyncFile::open(&temp_path).expect("Failed to open file");

        // Read from the file
        let (bytes_read, data) = async_file
            .read_at(0, 1024)
            .await
            .expect("Failed to read file");

        assert_eq!(bytes_read, test_data.len());
        assert_eq!(&data[..bytes_read], test_data);
    });
}

#[test]
fn test_async_file_create_write() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        // Create a temporary file path
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let temp_path = temp_dir.path().join("test_file.txt");

        // Create the file asynchronously
        let async_file = AsyncFile::create(&temp_path).expect("Failed to create file");

        // Write to the file
        let test_data = b"Hello, async file creation!";
        let bytes_written = async_file
            .write_at(0, test_data)
            .await
            .expect("Failed to write file");

        assert_eq!(bytes_written, test_data.len());

        // Verify the data was written
        let mut file =
            std::fs::File::open(&temp_path).expect("Failed to open file for verification");
        let mut contents = vec![];
        use std::io::Read;
        file.read_to_end(&mut contents)
            .expect("Failed to read file contents");
        assert_eq!(contents, test_data);
    });
}

/// `sync_data()` is the `fdatasync(2)` counterpart of `sync_all()`: it must
/// make written bytes readable from a fresh handle without waiting on any
/// later `sync_all()`.
#[test]
fn test_sync_data_makes_written_bytes_durable() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let temp_path = temp_dir.path().join("sync_data.txt");

        let async_file = AsyncFile::create(&temp_path).expect("Failed to create file");
        let test_data = b"fdatasync me";

        assert_eq!(
            async_file
                .write_at(0, test_data)
                .await
                .expect("Failed to write file"),
            test_data.len()
        );
        async_file.sync_data().await.expect("Failed to sync data");

        // A separate open of the same path sees the flushed bytes.
        assert_eq!(
            std::fs::read(&temp_path).expect("Failed to read file contents"),
            test_data
        );
    });
}

/// Many callers on one shard asking for a flush of the same file must all
/// complete, whichever syscall their group was served by. The syscall *count*
/// is asserted in `src/io/mio_poll.rs`, where the backend's per-shard counter
/// is reachable; here the point is that no waiter is dropped or misreported
/// when requests of both strengths share a group.
#[test]
fn test_grouped_flushes_of_one_file_all_complete() {
    const TASKS: usize = 16;

    let runtime = Runtime::new();

    runtime.block_on(async {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let temp_path = temp_dir.path().join("group_commit.txt");

        let file = Arc::new(AsyncFile::create(&temp_path).expect("Failed to create file"));
        let test_data = b"one file, many flushers";
        assert_eq!(
            file.write_at(0, test_data)
                .await
                .expect("Failed to write file"),
            test_data.len()
        );

        let flushes = (0..TASKS).map(|index| {
            let file = Arc::clone(&file);
            async move {
                // Alternate strengths: a `sync_all` caller must never be served
                // by a weaker group.
                if index % 2 == 0 {
                    file.sync_all().await
                } else {
                    file.sync_data().await
                }
                .unwrap_or_else(|error| panic!("task {index} flush failed: {error}"));
            }
        });
        futures::future::join_all(flushes).await;

        assert_eq!(
            std::fs::read(&temp_path).expect("Failed to read file contents"),
            test_data
        );
    });
}
