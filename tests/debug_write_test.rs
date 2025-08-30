//! Simple test to debug file write functionality.

use rust_miniss::{fs::AsyncFile, Runtime};

fn main() -> std::io::Result<()> {
    let runtime = Runtime::new();

    runtime.block_on(async {
        // Create a temporary file path
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let temp_path = temp_dir.path().join("test_file.txt");

        // Create the file asynchronously
        let async_file = AsyncFile::create(&temp_path).expect("Failed to create file");

        // Write to the file
        let test_data = b"Hello, async file creation!";
        println!("Writing {} bytes: {:?}", test_data.len(), test_data);
        let bytes_written = async_file
            .write_at(0, test_data)
            .await
            .expect("Failed to write file");
        println!("Wrote {} bytes", bytes_written);

        assert_eq!(bytes_written, test_data.len());

        // Sync the file to ensure data is written to disk
        async_file.sync_all().await.expect("Failed to sync file");

        // Verify the data was written
        let mut file =
            std::fs::File::open(&temp_path).expect("Failed to open file for verification");
        let mut contents = vec![];
        use std::io::Read;
        file.read_to_end(&mut contents)
            .expect("Failed to read file contents");
        println!("Read {} bytes: {:?}", contents.len(), contents);
        assert_eq!(contents, test_data);

        println!("Test passed!");
        Ok(())
    })
}
