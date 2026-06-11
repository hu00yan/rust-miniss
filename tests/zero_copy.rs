use futures::io::AsyncReadExt;
use rust_miniss::net::AsyncTcpStream;
use rust_miniss::Runtime;
use std::io::Write;
use std::net::TcpListener;

#[test]
fn test_tcp_read_data() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client.set_nonblocking(true).unwrap();
        client.write_all(b"hello world").unwrap();

        let (stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut async_stream = AsyncTcpStream::from(stream);

        let mut buf = [0u8; 64];
        let n = async_stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello world");
    });
}

#[test]
fn test_tcp_read_large_payload() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let runtime = Runtime::new();
    runtime.block_on(async {
        let payload = vec![0xAB_u8; 1024];
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client.set_nonblocking(true).unwrap();
        client.write_all(&payload).unwrap();

        let (stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut async_stream = AsyncTcpStream::from(stream);

        let mut buf = vec![0u8; 1024];
        let mut total = 0;
        while total < buf.len() {
            let n = async_stream.read(&mut buf[total..]).await.unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(total, 1024);
        assert_eq!(&buf[..total], &payload[..]);
    });
}

#[test]
fn test_tcp_read_empty_data() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client.set_nonblocking(true).unwrap();
        client.write_all(b"ping").unwrap();

        let (stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut async_stream = AsyncTcpStream::from(stream);

        use futures::io::AsyncReadExt;
        let mut buf = [0u8; 64];
        let n = async_stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping");
        drop(client);
    });
}
