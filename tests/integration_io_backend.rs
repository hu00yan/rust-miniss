use std::io::{Read, Seek, SeekFrom, Write};

#[test]
fn io_backend_read_file() {
    let runtime = rust_miniss::Runtime::new();
    runtime.block_on(async {
        let mut file = tempfile::tempfile().expect("temp file");
        let data = b"hello nextest";
        file.write_all(data).unwrap();
        file.flush().unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();

        let mut buf = vec![0u8; data.len()];
        file.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, data);
    });
}
