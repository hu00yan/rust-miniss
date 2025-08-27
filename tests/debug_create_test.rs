//! Test to debug AsyncFile::create functionality.

use rust_miniss::{fs::AsyncFile, Runtime};
use std::io::Read;

fn main() -> std::io::Result<()> {
    let runtime = Runtime::new();

    runtime.block_on(async {
        // Create a temporary file path
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let temp_path = temp_dir.path().join("test_file.txt");
        println!("Creating file at: {:?}", temp_path);

        // Create the file asynchronously
        let async_file = AsyncFile::create(&temp_path).expect("Failed to create file");
        println!("Created AsyncFile with fd: {}", async_file.as_raw_fd());

        // Write a simple string
        let test_data = b"test";
        println!("Writing {} bytes: {:?}", test_data.len(), test_data);
        let bytes_written = async_file.write_at(0, test_data).await.expect("Failed to write file");
        println!("Wrote {} bytes", bytes_written);
        
        assert_eq!(bytes_written, test_data.len());
        
        // Sync the file to ensure data is written to disk
        async_file.sync_all().await.expect("Failed to sync file");
        
        // Explicitly drop the file to ensure it's closed
        drop(async_file);
        
        // Give the system a moment to flush
        std::thread::sleep(std::time::Duration::from_millis(10));
        
        // Verify the file exists and has the correct size
        let metadata = std::fs::metadata(&temp_path).expect("Failed to get file metadata");
        println!("File size: {} bytes", metadata.len());
        assert_eq!(metadata.len(), test_data.len() as u64);
        
        // Verify the data was written
        let mut file = std::fs::File::open(&temp_path).expect("Failed to open file for verification");
        let mut contents = vec![];
        let bytes_read = file.read_to_end(&mut contents).expect("Failed to read file contents");
        println!("Read {} bytes: {:?}", bytes_read, contents);
        assert_eq!(contents, test_data);
        
        println!("Test passed!");
        Ok(())
    })
}