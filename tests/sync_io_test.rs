//! Test to verify synchronous file I/O works correctly.

use std::io::{Read, Write};

fn main() -> std::io::Result<()> {
    // Create a temporary file path
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let temp_path = temp_dir.path().join("test_file.txt");

    // Create and write to the file synchronously
    {
        let mut file = std::fs::File::create(&temp_path)?;
        let test_data = b"test";
        println!("Writing {} bytes: {:?}", test_data.len(), test_data);
        let bytes_written = file.write(test_data)?;
        println!("Wrote {} bytes", bytes_written);
        assert_eq!(bytes_written, test_data.len());
        
        // Sync the file
        file.sync_all()?;
    }

    // Read the file synchronously
    {
        let mut file = std::fs::File::open(&temp_path)?;
        let mut contents = vec![];
        let bytes_read = file.read_to_end(&mut contents)?;
        println!("Read {} bytes: {:?}", bytes_read, contents);
        assert_eq!(contents, b"test");
    }
    
    println!("Test passed!");
    Ok(())
}