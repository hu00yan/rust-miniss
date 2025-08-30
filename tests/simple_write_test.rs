//! Simple test to verify basic file write functionality.

use rust_miniss::{fs::AsyncFile, Runtime};
use std::io::Read;

fn main() -> std::io::Result<()> {
    let runtime = Runtime::new();

    let result = runtime.block_on(async {
        // Create a temporary file path
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let temp_path = temp_dir.path().join("test_file.txt");

        // Create the file asynchronously
        let async_file = AsyncFile::create(&temp_path).expect("Failed to create file");

        // Write a simple string
        let test_data = b"test";
        let bytes_written = async_file
            .write_at(0, test_data)
            .await
            .expect("Failed to write file");

        assert_eq!(bytes_written, test_data.len());

        // Sync the file to ensure data is written to disk
        async_file.sync_all().await.expect("Failed to sync file");

        // Explicitly drop the file to ensure it's closed
        drop(async_file);

        // Give the system a moment to flush
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Verify the file exists and has the correct size
        let metadata = std::fs::metadata(&temp_path).expect("Failed to get file metadata");
        assert_eq!(metadata.len(), test_data.len() as u64);

        // Verify the data was written
        let mut file =
            std::fs::File::open(&temp_path).expect("Failed to open file for verification");
        let mut contents = vec![];
        let _bytes_read = file
            .read_to_end(&mut contents)
            .expect("Failed to read file contents");
        assert_eq!(contents, test_data);

        println!("Test passed!");
        Ok(())
    });

    result
}
