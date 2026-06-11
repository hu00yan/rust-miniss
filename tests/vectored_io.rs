#![cfg(test)]

use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
use rust_miniss::Runtime;

#[test]
fn test_tcp_read_vectored_returns_pending() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
        let mut buf1 = [0u8; 64];
        let mut buf2 = [0u8; 64];
        let mut bufs = [
            std::io::IoSliceMut::new(&mut buf1),
            std::io::IoSliceMut::new(&mut buf2),
        ];
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);

        use rust_miniss::AsyncRead;
        let result = std::pin::Pin::new(&mut stream).poll_read_vectored(&mut cx, &mut bufs);
        assert!(
            matches!(result, std::task::Poll::Pending),
            "expected Pending on first poll, got {result:?}"
        );
    });
}

#[test]
fn test_tcp_write_vectored_returns_pending() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
        let bufs = [std::io::IoSlice::new(b"hello"), std::io::IoSlice::new(b"world")];
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);

        use rust_miniss::AsyncWrite;
        let result = std::pin::Pin::new(&mut stream).poll_write_vectored(&mut cx, &bufs);
        assert!(
            matches!(result, std::task::Poll::Pending),
            "expected Pending on first poll, got {result:?}"
        );
    });
}

#[test]
fn test_tcp_vectored_write_read_roundtrip() {
    use futures::io::{AsyncReadExt, AsyncWriteExt};

    let runtime = Runtime::new();
    runtime.block_on(async {
        let listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let mut writer = AsyncTcpStream::connect(addr).await.unwrap();
        let (mut reader, _) = listener.accept().await.unwrap();

        let bufs = [std::io::IoSlice::new(b"hello "), std::io::IoSlice::new(b"world")];
        let n = writer.write_vectored(&bufs).await.unwrap();
        assert_eq!(n, 11);

        let mut buf = [0u8; 64];
        let n = reader.read(&mut buf).await.unwrap();
        assert_eq!(n, 11);
        assert_eq!(&buf[..11], b"hello world");
    });
}
