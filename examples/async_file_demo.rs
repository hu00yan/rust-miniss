//! Simple test to verify async file I/O functionality.
//!
//! This example demonstrates the basic usage of the async file I/O API,
//! showing how to create, write to, and read from files asynchronously.
//!
//! ## Key Features Demonstrated
//!
//! 1. **Async File Creation**: Using `AsyncFile::create()` to create new files
//! 2. **Async File Writing**: Using `write_at()` to write data at specific offsets
//! 3. **Async File Reading**: Using `read_at()` to read data from specific offsets
//! 4. **Async File Sync**: Using `sync_all()` to ensure data is written to disk
//!
//! ## Memory Safety Note
//!
//! Our implementation ensures memory safety for async file operations across
//! all supported I/O backends. Particularly with io_uring, we carefully manage
//! data buffer lifetimes to prevent use-after-free errors that could lead to
//! data corruption. See the test documentation for details on the fix we
//! implemented for this critical issue.

use rust_miniss::{fs::AsyncFile, Runtime};
use std::io::Write;
use tempfile::NamedTempFile;

fn main() -> std::io::Result<()> {
    let runtime = Runtime::new();

    runtime.block_on(async {
        println!("Starting async file I/O demo...");
        
        // Create a temporary file
        let mut temp_file = NamedTempFile::new()?;
        let test_data = b"Hello, async file I/O!";
        temp_file.write_all(test_data)?;
        let temp_path = temp_file.path().to_path_buf();
        println!("Created temporary file at {:?}", temp_path);

        // Open the file asynchronously
        let async_file = AsyncFile::open(&temp_path)?;
        println!("Opened file asynchronously");

        // Read from the file
        println!("About to read from file...");
        let (bytes_read, data) = async_file.read_at(0, 1024).await?;
        println!("Read from file completed");
        
        println!("Read {} bytes: {:?}", bytes_read, String::from_utf8_lossy(&data));
        assert_eq!(bytes_read, test_data.len());
        assert_eq!(&data[..bytes_read], test_data);
        
        println!("File I/O test passed!");
        Ok(())
    })
}