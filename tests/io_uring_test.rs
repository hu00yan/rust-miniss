//! Simple test to verify io-uring Write operation works correctly with the same pattern as our project.

use io_uring::{opcode, types, IoUring};
use std::os::unix::io::AsRawFd;
use std::collections::HashMap;

enum PendingOp {
    WriteFile { op: (), data: Vec<u8> },
}

fn main() -> std::io::Result<()> {
    // Create a temporary file
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let temp_path = temp_dir.path().join("test_file.txt");
    
    // Create and open the file
    let file = std::fs::File::create(&temp_path)?;
    let fd = file.as_raw_fd();
    println!("Created file with fd: {}", fd);
    
    // Create io-uring instance
    let mut ring = IoUring::new(8)?;
    println!("Created io-uring instance");
    
    // Keep track of pending operations
    let mut pending_ops: HashMap<u64, PendingOp> = HashMap::new();
    
    // Write data to the file using the same pattern as our project
    let data = b"test".to_vec();
    let user_data = 1u64;
    
    println!("Data content: {:?}", data);
    println!("Data pointer: {:?}", data.as_ptr());
    
    let entry = opcode::Write::new(types::Fd(fd), data.as_ptr(), data.len() as _)
        .offset(0)
        .build()
        .user_data(user_data);
    
    // Store the data to keep it alive
    let cloned_data = data.clone();
    println!("Cloned data content: {:?}", cloned_data);
    println!("Cloned data pointer: {:?}", cloned_data.as_ptr());
    pending_ops.insert(user_data, PendingOp::WriteFile { op: (), data: cloned_data });
    
    // Submit the operation
    unsafe {
        ring.submission().push(&entry).expect("push sqe");
    }
    
    println!("Submitted write operation");
    
    // Submit and wait for completion
    ring.submit_and_wait(1)?;
    println!("Submitted and waited for completion");
    
    // Process completion
    let cqe = ring.completion().next().expect("completion");
    println!("Got CQE with user_data: {} and result: {}", cqe.user_data(), cqe.result());
    
    assert_eq!(cqe.user_data(), user_data);
    assert_eq!(cqe.result(), 4); // Should have written 4 bytes
    
    // Remove the pending operation
    pending_ops.remove(&cqe.user_data());
    
    // Close the file
    drop(file);
    
    // Read the file to verify the data was written correctly
    let mut file = std::fs::File::open(&temp_path)?;
    let mut contents = vec![];
    let bytes_read = std::io::Read::read_to_end(&mut file, &mut contents)?;
    println!("Read {} bytes: {:?}", bytes_read, contents);
    assert_eq!(contents, b"test");
    
    println!("Test passed!");
    Ok(())
}