use rust_miniss::compat::FuturesAsyncReadCompatExt;
use rust_miniss::net::AsyncTcpStream;
use rust_miniss::Runtime;
use std::io::Write;
use std::net::TcpListener;
use tokio::io::AsyncReadExt;

#[test]
fn test_tokio_io_compat_read() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client.set_nonblocking(true).unwrap();
        client.write_all(b"hello compat").unwrap();

        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true).unwrap();
                    let async_stream = AsyncTcpStream::from(stream);
                    let mut compat_stream = async_stream.compat();

                    let mut buf = [0u8; 64];
                    let n = compat_stream.read(&mut buf).await.unwrap();
                    assert_eq!(&buf[..n], b"hello compat");
                    break;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        }
    });
}

#[test]
fn test_compat_type_is_tokio_async_read() {
    fn assert_tokio_read<T: tokio::io::AsyncRead>(_t: &T) {}

    let runtime = Runtime::new();
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();

        let client = std::net::TcpStream::connect(addr).unwrap();
        client.set_nonblocking(true).unwrap();

        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true).unwrap();
                    let async_stream = AsyncTcpStream::from(stream);
                    let compat_stream = async_stream.compat();
                    assert_tokio_read(&compat_stream);
                    break;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        }
    });
}
