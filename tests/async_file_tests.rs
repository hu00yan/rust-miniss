//! Tests for async file I/O functionality.
//!
//! These exercise the paths the `mio` readiness backend implements through its
//! thread pool, so a failure here is a real one on a backend that does not
//! share the io_uring code path.

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

/// `write_rent_at` is the file-side rent shape — the one protocol that was
/// missing it. The caller hands over an owned buffer and gets that buffer back
/// with the byte count, the way `read_at` hands back the buffer it filled and
/// `AsyncTcpStream::write_rent` / `AsyncUdpSocket::send_to_rent` return theirs.
/// Two things must hold on every backend: the bytes land at the offset asked
/// for, and the buffer that comes back is immediately reusable rather than
/// one-shot. `write_at` is now a thin call through this path, so the existing
/// borrowed-shape tests above are its regression guardrail.
#[test]
fn test_write_rent_at_writes_at_the_offset_and_returns_a_reusable_buffer() {
    use rust_miniss::Buffer;

    let runtime = Runtime::new();

    runtime.block_on(async {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let temp_path = temp_dir.path().join("rent_write.txt");
        let file = AsyncFile::create(&temp_path).expect("Failed to create file");

        let first = Buffer::from_slice(b"head:");
        let (written, first) = file
            .write_rent_at(0, first)
            .await
            .expect("rent write at offset 0");
        assert_eq!(written, b"head:".len());
        assert_eq!(first.as_ref(), b"head:");

        // The returned buffer is the input of the next rent write. That reuse
        // is the point of the shape, not a side effect of it.
        let (written_second, _) = file
            .write_rent_at(b"head:".len() as u64, first)
            .await
            .expect("rent write at offset 5");
        assert_eq!(written_second, b"head:".len());

        // Read the file back through the owned-buffer read path: both segments
        // must be where their offsets put them. The read needs its own handle
        // because `create` is write-only and `open` is read-only, exactly as
        // `File::create` and `File::open` are; the read on a write-only handle
        // is the plain EBADF that POSIX specifies, not a backend defect.
        let reader = AsyncFile::open(&temp_path).expect("Failed to open for read-back");
        let (bytes_read, contents) = reader.read_at(0, 64).await.expect("read back");
        assert_eq!(bytes_read, 10);
        assert_eq!(&contents[..bytes_read], b"head:head:");
    });
}

/// On io_uring the operation owns the payload, so the buffer handed back is the
/// very allocation the caller handed over. The readiness backend cannot promise
/// that and does not: `src/io/mio_poll.rs` writes from an owned payload of its
/// own inside the thread-pool handoff, so what comes back there is a copy. The
/// bytes are asserted equal on both backends above; only the identity is
/// specific to this one, which is why the assertion is gated.
#[test]
#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
fn test_write_rent_at_returns_the_same_allocation_on_io_uring() {
    use rust_miniss::Buffer;

    let runtime = Runtime::new();

    runtime.block_on(async {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let temp_path = temp_dir.path().join("rent_identity.txt");
        let file = AsyncFile::create(&temp_path).expect("Failed to create file");

        let payload: &[u8] = b"same allocation, no copy";
        let owned = Buffer::from_slice(payload);
        let original_ptr = owned.as_ref().as_ptr();

        let (written, returned) = file
            .write_rent_at(0, owned)
            .await
            .expect("write rented payload");

        assert_eq!(written, payload.len());
        assert_eq!(
            returned.as_ref().as_ptr(),
            original_ptr,
            "write_rent_at must return the same allocation on io_uring, not a copy"
        );
        assert_eq!(returned.as_ref(), payload);
        assert_eq!(std::fs::read(&temp_path).expect("read file"), payload);
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
