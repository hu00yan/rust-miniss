use std::io::{Read, Seek, SeekFrom, Write};
use tempfile::tempfile;

// I/O backend abstraction test: behind feature flags run a simple file read test
#[tokio::test]
async fn io_backend_read_file() {
    // Prepare a temporary file with content
    let mut file = tempfile().expect("temp file");
    let data = b"hello nextest";
    file.write_all(data).unwrap();
    file.flush().unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();

    let mut buf = vec![0u8; data.len()];

    // Use standard blocking read here; purpose is to ensure test compiles under backends.
    // In a real integration, we'd call into the runtime's backend abstraction by feature.
    file.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, data);
}
